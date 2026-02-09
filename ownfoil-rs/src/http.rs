use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::header::{AUTHORIZATION, RANGE, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::auth::AuthSettings;
use crate::catalog::{Catalog, ContentFile, ContentKind, TitleVersions};
use crate::serve_files::{sanitize_relative_path, stream_with_range_support, FileServeError};

const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'?')
    .add(b'{')
    .add(b'}');

#[derive(Debug, Clone)]
pub struct AppState {
    pub catalog: Arc<RwLock<Catalog>>,
    pub library_root: PathBuf,
    pub auth: Arc<AuthSettings>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct ApiEntry {
    id: String,
    name: String,
    title_id: Option<String>,
    #[serde(rename = "titleid", skip_serializing_if = "Option::is_none")]
    titleid: Option<String>,
    #[serde(rename = "titleId", skip_serializing_if = "Option::is_none")]
    title_id_camel: Option<String>,
    version: Option<u32>,
    #[serde(rename = "ver", skip_serializing_if = "Option::is_none")]
    ver: Option<u32>,
    kind: ContentKind,
    #[serde(rename = "type")]
    content_type: ContentKind,
    size: u64,
    url: String,
}

#[derive(Debug, Serialize)]
struct CatalogResponse {
    total: usize,
    success: &'static str,
    files: Vec<ShopFile>,
    directories: Vec<String>,
    entries: Vec<ApiEntry>,
    sections: Vec<SectionInfo>,
}

#[derive(Debug, Serialize)]
struct SectionsResponse {
    sections: Vec<SectionInfo>,
}

#[derive(Debug, Clone, Serialize)]
struct SectionInfo {
    id: &'static str,
    label: &'static str,
}

#[derive(Debug, Serialize)]
struct ShopRootResponse {
    success: &'static str,
    files: Vec<ShopRootFile>,
}

#[derive(Debug, Serialize)]
struct ShopRootFile {
    url: String,
    size: u64,
}

#[derive(Debug, Serialize)]
struct ShopSectionsResponse {
    sections: Vec<ShopSection>,
}

#[derive(Debug, Serialize)]
struct ShopSection {
    id: &'static str,
    title: &'static str,
    items: Vec<ShopSectionItem>,
}

#[derive(Debug, Clone, Serialize)]
struct ShopSectionItem {
    name: String,
    title_name: String,
    title_id: Option<String>,
    app_id: String,
    app_version: String,
    app_type: &'static str,
    category: String,
    icon_url: String,
    url: String,
    size: u64,
    file_id: usize,
    filename: String,
    download_count: u64,
}

#[derive(Debug, Serialize)]
struct SearchResponse {
    query: String,
    success: &'static str,
    files: Vec<ShopFile>,
    entries: Vec<ApiEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct ShopFile {
    id: String,
    url: String,
    size: u64,
    name: String,
    title_id: Option<String>,
    #[serde(rename = "titleid", skip_serializing_if = "Option::is_none")]
    titleid: Option<String>,
    #[serde(rename = "titleId", skip_serializing_if = "Option::is_none")]
    title_id_camel: Option<String>,
    version: Option<u32>,
    #[serde(rename = "ver", skip_serializing_if = "Option::is_none")]
    ver: Option<u32>,
    kind: ContentKind,
    #[serde(rename = "type")]
    content_type: ContentKind,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: String,
}

#[derive(Debug, Deserialize)]
struct ShopSectionsQuery {
    limit: Option<usize>,
}

#[derive(Debug, Error)]
enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("title not found")]
    TitleNotFound,
    #[error("invalid path")]
    InvalidPath,
    #[error("not found")]
    NotFound,
    #[error("range not satisfiable")]
    InvalidRange,
    #[error("internal server error")]
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = self.to_string();
        let unauthorized = matches!(&self, ApiError::Unauthorized);
        let status = match &self {
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::TitleNotFound | ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::InvalidPath => StatusCode::BAD_REQUEST,
            ApiError::InvalidRange => StatusCode::RANGE_NOT_SATISFIABLE,
            ApiError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = Json(serde_json::json!({ "error": message }));
        let mut response = (status, body).into_response();
        if unauthorized {
            response.headers_mut().insert(
                WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"ownfoil-rs\""),
            );
        }
        response
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(shop_root))
        .route("/health", get(health))
        .route("/api/catalog", get(catalog_all))
        .route("/api/sections", get(sections))
        .route("/api/sections/{section}", get(section_entries))
        .route("/api/shop/sections", get(shop_sections))
        .route("/api/search", get(search))
        .route("/api/title/{title_id}/versions", get(title_versions))
        .route("/api/download/{*path}", get(download))
        .route("/api/get_game/{id}", get(download_by_id))
        .route("/api/titles", get(catalog_all))
        .route("/api/index", get(catalog_all))
        .route("/api/shop", get(shop_root))
        .route("/shop", get(shop_root))
        .route("/index", get(catalog_all))
        .route("/titles", get(catalog_all))
        .route("/download/{*path}", get(download))
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn shop_root(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ShopRootResponse>, ApiError> {
    ensure_authorized(&state, &headers)?;
    let catalog = state.catalog.read().await;
    let files = build_shop_root_files(catalog.files());
    debug!(files = files.len(), "shop root requested");
    Ok(Json(ShopRootResponse {
        success: "ok",
        files,
    }))
}

async fn catalog_all(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CatalogResponse>, ApiError> {
    ensure_authorized(&state, &headers)?;
    let catalog = state.catalog.read().await;
    let entries = map_entries(catalog.files());
    debug!(entries = entries.len(), "catalog requested");
    Ok(Json(build_catalog_response(entries)))
}

async fn sections(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SectionsResponse>, ApiError> {
    ensure_authorized(&state, &headers)?;
    debug!("sections requested");

    Ok(Json(SectionsResponse {
        sections: catalog_sections(),
    }))
}

async fn shop_sections(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ShopSectionsQuery>,
) -> Result<Json<ShopSectionsResponse>, ApiError> {
    ensure_authorized(&state, &headers)?;
    let limit = query.limit.unwrap_or(50).max(1);

    let catalog = state.catalog.read().await;
    let payload = build_shop_sections_payload(catalog.files(), limit);
    debug!(
        limit,
        sections = payload.sections.len(),
        "shop sections requested"
    );
    Ok(Json(payload))
}

async fn section_entries(
    State(state): State<AppState>,
    Path(section): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CatalogResponse>, ApiError> {
    ensure_authorized(&state, &headers)?;

    let catalog = state.catalog.read().await;
    let entries = match section.as_str() {
        "all" | "new" | "recommended" => map_entries(catalog.files()),
        "base" | "games" => map_entries_ref(&catalog.files_by_kind(ContentKind::Base)),
        "updates" | "update" => map_entries_ref(&catalog.files_by_kind(ContentKind::Update)),
        "dlc" => map_entries_ref(&catalog.files_by_kind(ContentKind::Dlc)),
        _ => Vec::new(),
    };
    debug!(section = %section, entries = entries.len(), "section requested");

    Ok(Json(build_catalog_response(entries)))
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, ApiError> {
    ensure_authorized(&state, &headers)?;

    let catalog = state.catalog.read().await;
    let matches = catalog.search(&params.q);
    debug!(query = %params.q, results = matches.len(), "search requested");
    let entries = map_entries_ref(&matches);

    Ok(Json(SearchResponse {
        query: params.q,
        success: "ok",
        files: map_shop_files(&entries),
        entries,
    }))
}

async fn title_versions(
    State(state): State<AppState>,
    Path(title_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<TitleVersions>, ApiError> {
    ensure_authorized(&state, &headers)?;

    let catalog = state.catalog.read().await;
    let versions = catalog.versions(&title_id).ok_or(ApiError::TitleNotFound)?;
    debug!(
        title_id = %versions.title_id,
        versions = versions.files.len(),
        "title versions requested"
    );
    Ok(Json(versions))
}

async fn download(
    State(state): State<AppState>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers)?;

    let decoded = percent_decode_str(&path)
        .decode_utf8()
        .map_err(|_| ApiError::InvalidPath)?;
    let sanitized = sanitize_relative_path(&decoded).map_err(map_file_error)?;
    let requested_range = headers
        .get(RANGE)
        .and_then(|raw| raw.to_str().ok())
        .map(str::to_string);

    let response = match stream_with_range_support(&state.library_root, &sanitized, &headers).await
    {
        Ok(response) => response,
        Err(error) => {
            warn!(
                path = %sanitized.display(),
                error = %error,
                "download failed"
            );
            return Err(map_file_error(error));
        }
    };
    info!(
        path = %sanitized.display(),
        status = %response.status(),
        range = ?requested_range,
        "download served"
    );

    Ok(response)
}

async fn download_by_id(
    State(state): State<AppState>,
    Path(id): Path<usize>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers)?;

    let (relative_path, filename) = {
        let catalog = state.catalog.read().await;
        let index = id.checked_sub(1).ok_or(ApiError::NotFound)?;
        let file = catalog.files().get(index).ok_or(ApiError::NotFound)?;
        (file.relative_path.clone(), file.name.clone())
    };

    let requested_range = headers
        .get(RANGE)
        .and_then(|raw| raw.to_str().ok())
        .map(str::to_string);
    let response =
        match stream_with_range_support(&state.library_root, &relative_path, &headers).await {
            Ok(response) => response,
            Err(error) => {
                warn!(
                    file_id = id,
                    filename = %filename,
                    path = %relative_path.display(),
                    error = %error,
                    "download by id failed"
                );
                return Err(map_file_error(error));
            }
        };

    info!(
        file_id = id,
        filename = %filename,
        path = %relative_path.display(),
        status = %response.status(),
        range = ?requested_range,
        "download by id served"
    );

    Ok(response)
}

fn map_entries(files: &[ContentFile]) -> Vec<ApiEntry> {
    files.iter().map(entry_to_api).collect::<Vec<_>>()
}

fn map_entries_ref(files: &[&ContentFile]) -> Vec<ApiEntry> {
    files
        .iter()
        .map(|entry| entry_to_api(entry))
        .collect::<Vec<_>>()
}

fn build_catalog_response(entries: Vec<ApiEntry>) -> CatalogResponse {
    CatalogResponse {
        success: "ok",
        total: entries.len(),
        files: map_shop_files(&entries),
        directories: Vec::new(),
        entries,
        sections: catalog_sections(),
    }
}

fn build_shop_root_files(files: &[ContentFile]) -> Vec<ShopRootFile> {
    files
        .iter()
        .enumerate()
        .map(|(index, file)| ShopRootFile {
            url: shop_game_url(index + 1, &file.name),
            size: file.size,
        })
        .collect::<Vec<_>>()
}

fn build_shop_sections_payload(files: &[ContentFile], limit: usize) -> ShopSectionsResponse {
    let indexed = files
        .iter()
        .enumerate()
        .map(|(index, file)| (index + 1, file))
        .collect::<Vec<_>>();

    let mut base_items = indexed
        .iter()
        .filter_map(|(index, file)| {
            if matches!(file.kind, ContentKind::Base | ContentKind::Unknown) {
                Some(to_shop_section_item(*index, file))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    base_items.sort_by(|left, right| right.file_id.cmp(&left.file_id));

    let mut update_items = indexed
        .iter()
        .filter_map(|(index, file)| {
            if file.kind == ContentKind::Update {
                Some(to_shop_section_item(*index, file))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    update_items.sort_by(|left, right| {
        parse_version_number(&right.app_version).cmp(&parse_version_number(&left.app_version))
    });

    let mut dlc_items = indexed
        .iter()
        .filter_map(|(index, file)| {
            if file.kind == ContentKind::Dlc {
                Some(to_shop_section_item(*index, file))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    dlc_items.sort_by(|left, right| {
        parse_version_number(&right.app_version).cmp(&parse_version_number(&left.app_version))
    });
    let dlc_items = dlc_items.into_iter().take(limit).collect::<Vec<_>>();

    let mut all_items = indexed
        .iter()
        .map(|(index, file)| to_shop_section_item(*index, file))
        .collect::<Vec<_>>();
    all_items.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));

    let mut new_items = base_items.iter().take(limit).cloned().collect::<Vec<_>>();
    if new_items.is_empty() {
        new_items = all_items.iter().take(limit).cloned().collect::<Vec<_>>();
    }
    let mut recommended_items = new_items.clone();
    if recommended_items.is_empty() {
        recommended_items = all_items.iter().take(limit).cloned().collect::<Vec<_>>();
    }

    ShopSectionsResponse {
        sections: vec![
            ShopSection {
                id: "new",
                title: "New",
                items: new_items,
            },
            ShopSection {
                id: "recommended",
                title: "Recommended",
                items: recommended_items,
            },
            ShopSection {
                id: "updates",
                title: "Updates",
                items: update_items,
            },
            ShopSection {
                id: "dlc",
                title: "DLC",
                items: dlc_items,
            },
            ShopSection {
                id: "all",
                title: "All",
                items: all_items,
            },
        ],
    }
}

fn to_shop_section_item(file_id: usize, file: &ContentFile) -> ShopSectionItem {
    let app_id = file
        .title_id
        .as_ref()
        .map(String::from)
        .unwrap_or_else(|| file.name.clone());
    let base_title_id = derive_base_title_id(file.kind, file.title_id.as_deref());
    let app_version = file
        .version
        .map(|version| version.to_string())
        .unwrap_or_else(|| String::from("0"));

    ShopSectionItem {
        name: file.name.clone(),
        title_name: file.name.clone(),
        title_id: base_title_id,
        app_id,
        app_version,
        app_type: app_type_for_kind(file.kind),
        category: String::new(),
        icon_url: String::new(),
        url: shop_game_url(file_id, &file.name),
        size: file.size,
        file_id,
        filename: file.name.clone(),
        download_count: 0,
    }
}

fn app_type_for_kind(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Base | ContentKind::Unknown => "BASE",
        ContentKind::Update => "UPDATE",
        ContentKind::Dlc => "DLC",
    }
}

fn derive_base_title_id(kind: ContentKind, title_id: Option<&str>) -> Option<String> {
    let raw = title_id?;
    if raw.len() != 16 || !raw.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }

    let normalized = raw.to_ascii_uppercase();
    match kind {
        ContentKind::Base | ContentKind::Unknown => Some(normalized),
        ContentKind::Update => {
            let mut chars = normalized.chars().collect::<Vec<_>>();
            let len = chars.len();
            chars[len - 3] = '0';
            chars[len - 2] = '0';
            chars[len - 1] = '0';
            Some(chars.into_iter().collect::<String>())
        }
        ContentKind::Dlc => {
            // Mirrors CyberFoil/Ownfoil logic: derive base from DLC app id by
            // decrementing the high part, then suffixing with 000.
            let high = &normalized[..13];
            let high_value = u64::from_str_radix(high, 16).ok()?;
            let base_high = high_value.checked_sub(1)?;
            Some(format!("{base_high:013X}000"))
        }
    }
}

fn parse_version_number(raw: &str) -> u64 {
    raw.parse::<u64>().unwrap_or(0)
}

fn shop_game_url(file_id: usize, filename: &str) -> String {
    format!("/api/get_game/{file_id}#{filename}")
}

fn map_shop_files(entries: &[ApiEntry]) -> Vec<ShopFile> {
    entries
        .iter()
        .map(|entry| ShopFile {
            id: entry.id.clone(),
            url: entry.url.clone(),
            size: entry.size,
            name: entry.name.clone(),
            title_id: entry.title_id.clone(),
            titleid: entry.title_id.clone(),
            title_id_camel: entry.title_id.clone(),
            version: entry.version,
            ver: entry.version,
            kind: entry.kind,
            content_type: entry.kind,
        })
        .collect::<Vec<_>>()
}

fn catalog_sections() -> Vec<SectionInfo> {
    vec![
        SectionInfo {
            id: "new",
            label: "New",
        },
        SectionInfo {
            id: "recommended",
            label: "Recommended",
        },
        SectionInfo {
            id: "updates",
            label: "Updates",
        },
        SectionInfo {
            id: "dlc",
            label: "DLC",
        },
        SectionInfo {
            id: "all",
            label: "All",
        },
    ]
}

fn entry_to_api(file: &ContentFile) -> ApiEntry {
    let rel = file.relative_path.to_string_lossy();
    let encoded_segments = rel
        .split('/')
        .map(|segment| utf8_percent_encode(segment, PATH_SEGMENT_ENCODE_SET).to_string())
        .collect::<Vec<_>>()
        .join("/");

    ApiEntry {
        id: rel.to_string(),
        name: file.name.clone(),
        title_id: file.title_id.clone(),
        titleid: file.title_id.clone(),
        title_id_camel: file.title_id.clone(),
        version: file.version,
        ver: file.version,
        kind: file.kind,
        content_type: file.kind,
        size: file.size,
        url: format!("/download/{encoded_segments}"),
    }
}

fn ensure_authorized(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if !state.auth.is_enabled() {
        return Ok(());
    }

    if let Some((username, password)) = extract_basic_auth(headers) {
        if state.auth.is_authorized(&username, &password) {
            debug!("authorized request using basic auth");
            return Ok(());
        }
    }

    warn!("unauthorized request");
    Err(ApiError::Unauthorized)
}

fn extract_basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let raw = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())?;
    let encoded = raw
        .strip_prefix("Basic ")
        .or_else(|| raw.strip_prefix("basic "))
        .map(str::trim)
        .unwrap_or_else(|| raw.trim());
    let decoded = decode_base64(encoded)?;
    let credentials = String::from_utf8(decoded).ok()?;
    let (username, password) = credentials.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn value_of(ch: char) -> Option<u8> {
        match ch {
            'A'..='Z' => Some((ch as u8) - b'A'),
            'a'..='z' => Some((ch as u8) - b'a' + 26),
            '0'..='9' => Some((ch as u8) - b'0' + 52),
            '+' => Some(62),
            '/' => Some(63),
            _ => None,
        }
    }

    let cleaned = input.chars().filter(|ch| !ch.is_ascii_whitespace());
    let mut out = Vec::new();
    let mut chunk = [0_u8; 4];
    let mut pad = 0_u8;
    let mut idx = 0_usize;

    for ch in cleaned {
        if ch == '=' {
            chunk[idx] = 0;
            pad = pad.saturating_add(1);
        } else {
            chunk[idx] = value_of(ch)?;
        }
        idx += 1;

        if idx == 4 {
            if pad > 2 {
                return None;
            }

            let b0 = (chunk[0] << 2) | (chunk[1] >> 4);
            out.push(b0);

            if pad < 2 {
                let b1 = (chunk[1] << 4) | (chunk[2] >> 2);
                out.push(b1);
            }
            if pad == 0 {
                let b2 = (chunk[2] << 6) | chunk[3];
                out.push(b2);
            }

            chunk = [0; 4];
            pad = 0;
            idx = 0;
        }
    }

    if idx != 0 {
        return None;
    }

    Some(out)
}

fn map_file_error(error: FileServeError) -> ApiError {
    match error {
        FileServeError::InvalidPath => ApiError::InvalidPath,
        FileServeError::NotFound => ApiError::NotFound,
        FileServeError::InvalidRange => ApiError::InvalidRange,
        FileServeError::Io(_) | FileServeError::HeaderValue(_) => ApiError::Internal,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::Result;
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use serde_json::Value;
    use tempfile::tempdir;
    use tokio::fs;
    use tokio::sync::RwLock;

    use crate::auth::{AuthSettings, AuthUser};
    use crate::catalog::{Catalog, ContentFile, ContentKind};

    use super::{router, AppState};

    #[tokio::test]
    async fn download_supports_range() -> Result<()> {
        let dir = tempdir()?;
        let file_path = dir.path().join("demo.nsp");
        fs::write(&file_path, b"0123456789").await?;

        let state = AppState {
            catalog: Arc::new(RwLock::new(Catalog::from_files(Vec::new()))),
            library_root: dir.path().to_path_buf(),
            auth: Arc::new(AuthSettings::from_users(Vec::new())),
        };

        let server = TestServer::new(router(state))?;

        let response = server
            .get("/api/download/demo.nsp")
            .add_header("Range", "bytes=1-3")
            .await;

        assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.header("accept-ranges"), "bytes");
        assert_eq!(response.text(), "123");
        Ok(())
    }

    #[tokio::test]
    async fn get_game_by_id_supports_range() -> Result<()> {
        let dir = tempdir()?;
        let file_path = dir.path().join("demo.nsp");
        fs::write(&file_path, b"0123456789").await?;

        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
        }]);

        let state = AppState {
            catalog: Arc::new(RwLock::new(catalog)),
            library_root: dir.path().to_path_buf(),
            auth: Arc::new(AuthSettings::from_users(Vec::new())),
        };

        let server = TestServer::new(router(state))?;
        let response = server
            .get("/api/get_game/1")
            .add_header("Range", "bytes=1-3")
            .await;

        assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.header("accept-ranges"), "bytes");
        assert_eq!(response.text(), "123");
        Ok(())
    }

    #[tokio::test]
    async fn catalog_requires_basic_auth_when_enabled() -> Result<()> {
        let state = AppState {
            catalog: Arc::new(RwLock::new(Catalog::from_files(Vec::new()))),
            library_root: std::env::temp_dir(),
            auth: Arc::new(AuthSettings::from_users(vec![AuthUser {
                username: String::from("admin"),
                password: String::from("secret"),
            }])),
        };

        let server = TestServer::new(router(state))?;

        let unauthorized = server.get("/api/catalog").await;
        assert_eq!(unauthorized.status_code(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthorized.header("www-authenticate"),
            "Basic realm=\"ownfoil-rs\""
        );

        let authorized = server
            .get("/api/catalog")
            .add_header("Authorization", "Basic YWRtaW46d3Jvbmc=")
            .await;
        assert_eq!(authorized.status_code(), StatusCode::UNAUTHORIZED);

        let authorized = server
            .get("/api/catalog")
            .add_header("Authorization", "Basic YWRtaW46c2VjcmV0")
            .await;
        assert_eq!(authorized.status_code(), StatusCode::OK);

        let authorized = server
            .get("/api/catalog")
            .add_header("Authorization", "YWRtaW46c2VjcmV0")
            .await;
        assert_eq!(authorized.status_code(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn shop_response_contains_files_list() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
        }]);

        let state = AppState {
            catalog: Arc::new(RwLock::new(catalog)),
            library_root: std::env::temp_dir(),
            auth: Arc::new(AuthSettings::from_users(Vec::new())),
        };

        let server = TestServer::new(router(state))?;
        let response = server.get("/shop").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let files = body
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(files.len(), 1);
        let first = files[0].as_object().cloned().unwrap_or_default();
        assert_eq!(
            first.get("url"),
            Some(&Value::String(String::from("/api/get_game/1#demo.nsp")))
        );
        assert_eq!(first.get("size"), Some(&Value::Number(10_u64.into())));
        assert_eq!(
            body.get("success"),
            Some(&Value::String(String::from("ok")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn root_response_contains_files_list() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
        }]);

        let state = AppState {
            catalog: Arc::new(RwLock::new(catalog)),
            library_root: std::env::temp_dir(),
            auth: Arc::new(AuthSettings::from_users(Vec::new())),
        };

        let server = TestServer::new(router(state))?;
        let response = server.get("/").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let files = body
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(files.len(), 1);
        let first = files[0].as_object().cloned().unwrap_or_default();
        assert_eq!(
            first.get("url"),
            Some(&Value::String(String::from("/api/get_game/1#demo.nsp")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn shop_sections_returns_section_items() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
        }]);

        let state = AppState {
            catalog: Arc::new(RwLock::new(catalog)),
            library_root: std::env::temp_dir(),
            auth: Arc::new(AuthSettings::from_users(Vec::new())),
        };

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/sections").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let sections = body
            .get("sections")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(sections.len(), 5);

        let first_section = sections
            .first()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            first_section.get("id"),
            Some(&Value::String(String::from("new")))
        );
        let items = first_section
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(items.len(), 1);

        let first_item = items[0].as_object().cloned().unwrap_or_default();
        assert_eq!(
            first_item.get("url"),
            Some(&Value::String(String::from("/api/get_game/1#demo.nsp")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn shop_sections_new_falls_back_to_all_when_no_base_items() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("update.nsp"),
            name: String::from("update.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000800")),
            version: Some(65536),
            kind: ContentKind::Update,
        }]);

        let state = AppState {
            catalog: Arc::new(RwLock::new(catalog)),
            library_root: std::env::temp_dir(),
            auth: Arc::new(AuthSettings::from_users(Vec::new())),
        };

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/sections").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let sections = body
            .get("sections")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let new_items = sections
            .iter()
            .find(|section| section.get("id") == Some(&Value::String(String::from("new"))))
            .and_then(|section| section.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(new_items.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn update_section_item_uses_base_title_id_and_update_app_id() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("update.nsp"),
            name: String::from("update.nsp"),
            size: 10,
            title_id: Some(String::from("0100ABCD12340800")),
            version: Some(65536),
            kind: ContentKind::Update,
        }]);

        let state = AppState {
            catalog: Arc::new(RwLock::new(catalog)),
            library_root: std::env::temp_dir(),
            auth: Arc::new(AuthSettings::from_users(Vec::new())),
        };

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/sections").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let updates = body
            .get("sections")
            .and_then(Value::as_array)
            .and_then(|sections| {
                sections.iter().find(|section| {
                    section.get("id") == Some(&Value::String(String::from("updates")))
                })
            })
            .and_then(|section| section.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        assert_eq!(updates.len(), 1);
        let item = updates[0].as_object().cloned().unwrap_or_default();
        assert_eq!(
            item.get("title_id"),
            Some(&Value::String(String::from("0100ABCD12340000")))
        );
        assert_eq!(
            item.get("app_id"),
            Some(&Value::String(String::from("0100ABCD12340800")))
        );
        assert_eq!(
            item.get("app_type"),
            Some(&Value::String(String::from("UPDATE")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn dlc_section_item_uses_base_title_id_and_dlc_app_id() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("dlc.nsp"),
            name: String::from("dlc.nsp"),
            size: 10,
            title_id: Some(String::from("0100ABCD12341001")),
            version: Some(0),
            kind: ContentKind::Dlc,
        }]);

        let state = AppState {
            catalog: Arc::new(RwLock::new(catalog)),
            library_root: std::env::temp_dir(),
            auth: Arc::new(AuthSettings::from_users(Vec::new())),
        };

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/sections").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let dlc = body
            .get("sections")
            .and_then(Value::as_array)
            .and_then(|sections| {
                sections
                    .iter()
                    .find(|section| section.get("id") == Some(&Value::String(String::from("dlc"))))
            })
            .and_then(|section| section.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        assert_eq!(dlc.len(), 1);
        let item = dlc[0].as_object().cloned().unwrap_or_default();
        assert_eq!(
            item.get("title_id"),
            Some(&Value::String(String::from("0100ABCD12340000")))
        );
        assert_eq!(
            item.get("app_id"),
            Some(&Value::String(String::from("0100ABCD12341001")))
        );
        assert_eq!(
            item.get("app_type"),
            Some(&Value::String(String::from("DLC")))
        );
        Ok(())
    }
}
