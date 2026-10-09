//! The native Ownfoil protocol used by Sphaira 1.0.8 and later.
use super::{
    AppState,
    auth::{Access, ensure_access, ensure_authorized},
    error::ApiError,
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use axum_extra::extract::CookieJar;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Cursor, sync::LazyLock, time::Duration};

pub async fn handshake(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    ensure_authorized(&state, &headers, super::handlers::session_token(&jar)).await?;
    let settings = state.settings.read().await.clone();
    Ok(Json(json!({"uid":settings.server.uid, "name":settings.shop.name,
        "version":env!("CARGO_PKG_VERSION"), "protocol_version":1,
        "motd":settings.shop.motd, "public":settings.shop.public, "remote":settings.shop.host,
        "features":{"shop":true,"resumable_download":true,"dumps_upload":false,
                    "save_backup":false,"resumable_upload":false}})))
}

pub async fn services_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    ensure_access(&state, &headers, super::handlers::session_token(&jar), Access::Admin).await?;
    super::handlers::ensure_same_origin(&headers)?;
    let Some(enabled) = body["discovery"]["enabled"].as_bool() else {
        return Ok(Json(
            json!({"success":false,"errors":[{"path":"services.discovery.enabled","error":"Expected a boolean"}]}),
        ));
    };
    let mut settings = state.settings.write().await;
    let mut candidate = settings.clone();
    candidate.services.discovery.enabled = enabled;
    candidate.save(&state.settings_path).map_err(|_| ApiError::Internal)?;
    *settings = candidate;
    drop(settings);
    Ok(Json(json!({"success":true,"errors":[]})))
}

fn source<'a>(title: &'a Value, kind: &str, position: usize) -> Option<&'a str> {
    let url = match kind {
        "icon" => title["iconUrl"].as_str(),
        "banner" => title["bannerUrl"].as_str(),
        "screenshot" => title["screenshots"].as_array()?.get(position)?.as_str(),
        _ => None,
    }?;
    (!url.is_empty()).then_some(url)
}
fn source_hash(url: &str) -> String {
    hex::encode(Sha256::digest(url.as_bytes()))
}
fn image(title: &Value, kind: &str, position: usize, size: &str) -> Value {
    source(title, kind, position).map_or(Value::Null, |url| json!({
        "url":format!("/api/media/{}/{kind}/{position}/{}/{}.jpg",title["titleId"].as_str().unwrap_or_default(),size.to_ascii_lowercase(),source_hash(url)),
        "size":size,"local":true
    }))
}
pub fn images(title: &Value, field: &str, args: &Value) -> Value {
    let size = args["size"].as_str().unwrap_or("CLIENT");
    if field == "screenshots" {
        return title["screenshots"].as_array().map_or(Value::Null, |urls| {
            json!(
                (0..urls.len())
                    .map(|i| image(title, "screenshot", i, size))
                    .filter(|v| !v.is_null())
                    .collect::<Vec<_>>()
            )
        });
    }
    image(title, field, 0, size)
}

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default()
});
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

pub async fn media(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((title, kind, position, size, hash)): Path<(String, String, usize, String, String)>,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers, super::handlers::session_token(&jar)).await?;
    if !matches!(size.as_str(), "original" | "thumb" | "client" | "screen") {
        return Err(ApiError::NotFound);
    }
    let info = state.titledb.lookup(&title).await.ok_or(ApiError::NotFound)?;
    let mut record = Value::Object(info.record);
    record["iconUrl"] = json!(info.icon_url);
    record["bannerUrl"] = json!(info.banner_url);
    if let Some(raw) = record["screenshots"].as_str() {
        record["screenshots"] = serde_json::from_str(raw).unwrap_or(Value::Null);
    }
    let url = source(&record, &kind, position).ok_or(ApiError::NotFound)?.to_string();
    if hash != format!("{}.jpg", source_hash(&url))
        || !(url.starts_with("https://") || url.starts_with("http://"))
    {
        return Err(ApiError::NotFound);
    }
    let directory = state.data_dir.join("media").join(&kind).join(&size);
    let path = directory.join(&hash);
    let etag = format!("\"{hash}\"");
    if path.is_file() && headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(&etag) {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [("etag", etag), ("cache-control", "private, max-age=31536000, immutable".into())],
        )
            .into_response());
    }
    let bytes = if let Ok(bytes) = tokio::fs::read(&path).await {
        bytes
    } else {
        let original_dir = state.data_dir.join("media").join("source");
        let original_path = original_dir.join(&hash);
        let bytes = if let Ok(bytes) = tokio::fs::read(&original_path).await {
            bytes
        } else {
            let bytes = fetch_image(&url).await?;
            save_cache(&original_dir, &original_path, &bytes).await?;
            bytes
        };
        let icon = kind == "icon";
        let bytes = tokio::task::spawn_blocking(move || render(&bytes, icon, &size))
            .await
            .map_err(|_| ApiError::Internal)?
            .map_err(|_| ApiError::NotFound)?;
        save_cache(&directory, &path, &bytes).await?;
        bytes
    };
    Ok((
        [
            ("content-type", "image/jpeg".to_string()),
            ("etag", etag),
            ("cache-control", "private, max-age=31536000, immutable".to_string()),
            ("vary", "Authorization, Cookie".to_string()),
        ],
        bytes,
    )
        .into_response())
}
fn render(bytes: &[u8], icon: bool, size: &str) -> anyhow::Result<Vec<u8>> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?;
    let bounds = match (icon, size) {
        (true, "thumb") => Some((176, 176)),
        (false, "thumb") => Some((320, 180)),
        (true, "client") => Some((256, 256)),
        (false, "client") => Some((720, 405)),
        (true, "screen") => Some((720, 720)),
        (false, "screen") => Some((1280, 720)),
        _ => None,
    };
    let image = if let Some((width, height)) = bounds {
        if image.width() > width || image.height() > height {
            image.resize(width, height, image::imageops::FilterType::Lanczos3)
        } else {
            image
        }
    } else {
        image
    };
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&image.to_rgb8())?;
    Ok(out)
}

async fn fetch_image(url: &str) -> Result<Vec<u8>, ApiError> {
    let mut reply = CLIENT
        .get(url)
        .send()
        .await
        .map_err(|_| ApiError::NotFound)?
        .error_for_status()
        .map_err(|_| ApiError::NotFound)?;
    if reply.content_length().is_some_and(|n| n > MAX_IMAGE_BYTES as u64) {
        return Err(ApiError::NotFound);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = reply.chunk().await.map_err(|_| ApiError::NotFound)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_IMAGE_BYTES {
            return Err(ApiError::NotFound);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn save_cache(
    directory: &std::path::Path,
    path: &std::path::Path,
    bytes: &[u8],
) -> Result<(), ApiError> {
    tokio::fs::create_dir_all(directory).await.map_err(|_| ApiError::Internal)?;
    let temporary = directory.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(&temporary, bytes).await.map_err(|_| ApiError::Internal)?;
    let result = tokio::fs::rename(&temporary, path).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(temporary).await;
    }
    result.map_err(|_| ApiError::Internal)
}
