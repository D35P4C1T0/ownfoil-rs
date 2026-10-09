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

fn image(title: &Value, kind: &str, position: usize, size: &str) -> Value {
    let source = crate::media::source(title, kind, position);
    if title["_localMediaEnabled"] == false {
        if let Some(url) = source.filter(|url| !url.starts_with("/api/media/")) {
            return json!({"url":url,"size":size,"local":false,"width":null,"height":null});
        }
    }
    let slot = title["_media"].as_array().and_then(|slots| {
        slots.iter().find(|slot| {
            slot["kind"] == kind
                && slot["position"] == position
                && source.is_some_and(|url| {
                    slot["source"] == url
                        || (url.starts_with("/api/media/")
                            && url
                                .rsplit('/')
                                .next()
                                .is_some_and(|filename| slot["filename"] == filename))
                })
        })
    });
    let filename = slot
        .and_then(|slot| slot["filename"].as_str())
        .map(str::to_owned)
        .or_else(|| source.map(|url| format!("{}.jpg", crate::media::source_hash(url))));
    let Some(filename) = filename else { return Value::Null };
    let (width, height) = slot.map_or((0, 0), |slot| {
        crate::media::fit(
            u32::try_from(slot["width"].as_u64().unwrap_or(0)).unwrap_or(0),
            u32::try_from(slot["height"].as_u64().unwrap_or(0)).unwrap_or(0),
            kind,
            &size.to_ascii_lowercase(),
        )
    });
    let url = source.filter(|url| url.starts_with("/api/media/")).map_or_else(
        || {
            format!(
                "/api/media/{}/{kind}/{position}/{}/{filename}",
                title["titleId"].as_str().unwrap_or_default(),
                size.to_ascii_lowercase()
            )
        },
        |url| {
            let parts: Vec<_> = url.split('/').collect();
            if parts.len() == 8 {
                format!(
                    "/api/media/{}/{}/{}/{}/{}",
                    parts[3],
                    parts[4],
                    parts[5],
                    size.to_ascii_lowercase(),
                    parts[7]
                )
            } else {
                url.to_owned()
            }
        },
    );
    json!({"url":url,"size":size,"local":true,"width":if width==0 {Value::Null} else {json!(width)},"height":if height==0 {Value::Null} else {json!(height)}})
}
pub fn images(title: &Value, field: &str, args: &Value) -> Value {
    let size = args["size"].as_str().unwrap_or("CLIENT");
    if field == "screenshots" {
        if !title["screenshots"].is_array() {
            return Value::Null;
        }
        let count = title["screenshots"].as_array().map_or(0, Vec::len);
        return json!(
            (0..count)
                .map(|i| image(title, "screenshot", i, size))
                .filter(|v| !v.is_null())
                .collect::<Vec<_>>()
        );
    }
    image(title, field, 0, size)
}

pub async fn media(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Path((title, kind, position, size, hash)): Path<(String, String, usize, String, String)>,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers, super::handlers::session_token(&jar)).await?;
    if !crate::media::SIZES.contains(&size.as_str())
        || !crate::media::KINDS.contains(&kind.as_str())
        || hash.len() != 68
        || hash.strip_suffix(".jpg").is_none()
        || !hash.as_bytes().iter().take(64).all(u8::is_ascii_hexdigit)
    {
        return Err(ApiError::NotFound);
    }
    let path = state.data_dir.join("media").join(&kind).join(&size).join(&hash);
    let etag = format!("\"{hash}\"");
    if !path.is_file() {
        let info = state.titledb.lookup(&title).await.ok_or(ApiError::NotFound)?;
        let mut record = Value::Object(info.record);
        record["iconUrl"] = json!(info.icon_url);
        record["bannerUrl"] = json!(info.banner_url);
        if let Some(raw) = record["screenshots"].as_str() {
            record["screenshots"] = serde_json::from_str(raw).unwrap_or(Value::Null);
        }
        let url = crate::media::source(&record, &kind, position).ok_or(ApiError::NotFound)?;
        if hash != format!("{}.jpg", crate::media::source_hash(url)) {
            return Err(ApiError::NotFound);
        }
        crate::media::download(&state, &title, &kind, position, url)
            .await
            .map_err(|_| ApiError::NotFound)?;
    }
    if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(&etag) {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [
                ("etag", etag),
                ("cache-control", "private, max-age=31536000, immutable".into()),
                ("vary", "Authorization, Cookie".into()),
            ],
        )
            .into_response());
    }
    let bytes = tokio::fs::read(path).await.map_err(|_| ApiError::NotFound)?;
    let content_type = match image::guess_format(&bytes) {
        Ok(image::ImageFormat::Png) => "image/png",
        Ok(image::ImageFormat::WebP) => "image/webp",
        _ => "image/jpeg",
    };
    Ok((
        [
            ("content-type", content_type.to_string()),
            ("etag", etag),
            ("cache-control", "private, max-age=31536000, immutable".to_string()),
            ("vary", "Authorization, Cookie".to_string()),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_sources_do_not_override_current_urls_or_resurrect_screenshots() {
        let title = json!({"titleId":"0100000000000000","iconUrl":"https://new/icon","screenshots":[],"_media":[{"kind":"icon","position":0,"source":"https://old/icon","filename":"old.jpg","width":512,"height":512},{"kind":"screenshot","position":0,"source":"old","filename":"old.jpg","width":100,"height":100}]});
        let icon = images(&title, "icon", &json!({"size":"THUMB"}));
        assert!(
            icon["url"]
                .as_str()
                .unwrap_or_default()
                .contains(&crate::media::source_hash("https://new/icon"))
        );
        assert!(icon["width"].is_null());
        let mut removed = title.clone();
        removed["iconUrl"] = Value::Null;
        assert!(images(&removed, "icon", &json!({})).is_null());
        assert_eq!(images(&title, "screenshots", &json!({})), json!([]));
    }
    #[test]
    fn disabled_local_media_returns_remote_sources() {
        let title = json!({"titleId":"0100000000000000","iconUrl":"https://source/icon","_localMediaEnabled":false});
        let image = images(&title, "icon", &json!({"size":"THUMB"}));
        assert_eq!(image["url"], "https://source/icon");
        assert_eq!(image["local"], false);
    }
    #[test]
    fn persisted_dimensions_are_projected_for_requested_rendition() {
        let title = json!({"titleId":"0100000000000000","bannerUrl":"https://image","_media":[{"kind":"banner","position":0,"source":"https://image","filename":"image.jpg","width":1920,"height":1080}]});
        let image = images(&title, "banner", &json!({"size":"CLIENT"}));
        assert_eq!(image["width"], 720);
        assert_eq!(image["height"], 405);
    }
}
