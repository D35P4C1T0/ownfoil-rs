use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use axum_extra::extract::Form;
use futures_util::stream::StreamExt;
use percent_encoding::percent_decode_str;
use tower_governor::{
    governor::GovernorConfigBuilder, key_extractor::GlobalKeyExtractor, GovernorLayer,
};
use tracing::{debug, warn};

use crate::catalog::{ContentKind, TitleVersions};
use crate::serve_files::{sanitize_relative_path, stream_with_range_support, DownloadLogContext};

use crate::config::TitleDbConfig;

use super::auth::ensure_authorized;
use super::error::ApiError;

const SESSION_COOKIE: &str = "ownfoil_session";

/// Extracts peer address from request extensions when available (e.g. from
/// `into_make_service_with_connect_info`). Returns `None` in tests or when
/// connection info is not set.
struct PeerAddr(pub Option<SocketAddr>);

impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let addr = parts.extensions.get::<SocketAddr>().copied().or_else(|| {
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<SocketAddr>>()
                .map(|c| c.0)
        });
        Ok(PeerAddr(addr))
    }
}

use super::responses::{
    build_catalog_response, build_shop_root_files, build_shop_sections_payload, catalog_sections,
    map_file_error, map_shop_files, map_to_entries, static_png_response, CatalogResponse,
    HealthResponse, SavesListResponse, SearchQuery, SearchResponse, SectionsResponse,
    ShopRootResponse, ShopSectionsQuery, ShopSectionsResponse,
};
use super::state::AppState;

/// Build the Axum router with all routes, layers (rate limit, request ID, trace), and state.
pub fn router(state: AppState) -> Router {
    let governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(20)
            .burst_size(50)
            .key_extractor(GlobalKeyExtractor)
            .finish()
            .unwrap_or_else(|| panic!("default governor config is valid")),
    );

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
        .route("/api/shop/icon/{title_id}", get(shop_icon))
        .route("/api/shop/banner/{title_id}", get(shop_banner))
        .route("/api/saves/list", get(saves_list))
        .route("/api/titles", get(catalog_all))
        .route("/api/index", get(catalog_all))
        .route("/api/shop", get(shop_root))
        .route("/shop", get(shop_root))
        .route("/index", get(catalog_all))
        .route("/titles", get(catalog_all))
        .route("/download/{*path}", get(download))
        .route("/admin", get(admin_ui))
        .route("/admin/settings", get(settings_ui))
        .route("/admin/login", get(login_page).post(login_post))
        .route("/admin/logout", get(logout))
        .route("/api/settings", get(settings_get).post(settings_post))
        .route("/api/settings/refresh", post(settings_refresh))
        .route("/api/settings/titledb/progress", get(titledb_progress_sse))
        .route("/api/settings/titledb/test", get(titledb_test_connectivity))
        .layer(GovernorLayer::new(governor_conf))
        .layer(tower_http::request_id::SetRequestIdLayer::new(
            axum::http::header::HeaderName::from_static("x-request-id"),
            tower_http::request_id::MakeRequestUuid,
        ))
        .layer(tower_http::request_id::PropagateRequestIdLayer::new(
            axum::http::header::HeaderName::from_static("x-request-id"),
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let catalog_files = state.catalog.read().await.files().len();
    Json(HealthResponse {
        status: "ok",
        catalog_files: Some(catalog_files),
    })
}

async fn shop_root(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<ShopRootResponse>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
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
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<CatalogResponse>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
    let catalog = state.catalog.read().await;
    let entries = map_to_entries(catalog.files());
    debug!(entries = entries.len(), "catalog requested");
    Ok(Json(build_catalog_response(entries)))
}

async fn sections(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<SectionsResponse>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
    debug!("sections requested");
    Ok(Json(SectionsResponse {
        sections: catalog_sections(),
    }))
}

async fn shop_sections(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<ShopSectionsQuery>,
    headers: HeaderMap,
) -> Result<Json<ShopSectionsResponse>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
    let limit = query.limit.unwrap_or(50).max(1);

    let catalog = state.catalog.read().await;
    let payload = build_shop_sections_payload(catalog.files(), limit, &state.titledb).await;
    debug!(
        limit,
        sections = payload.sections.len(),
        "shop sections requested"
    );
    Ok(Json(payload))
}

async fn section_entries(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(section): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CatalogResponse>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;

    let catalog = state.catalog.read().await;
    let entries = match section.as_str() {
        "all" | "new" | "recommended" => map_to_entries(catalog.files()),
        "base" | "games" => map_to_entries(catalog.files_by_kind(ContentKind::Base)),
        "updates" | "update" => map_to_entries(catalog.files_by_kind(ContentKind::Update)),
        "dlc" => map_to_entries(catalog.files_by_kind(ContentKind::Dlc)),
        _ => Vec::new(),
    };
    debug!(section = %section, entries = entries.len(), "section requested");

    Ok(Json(build_catalog_response(entries)))
}

async fn search(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(params): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;

    let catalog = state.catalog.read().await;
    let matches = catalog.search(&params.q);
    debug!(query = %params.q, results = matches.len(), "search requested");
    let entries = map_to_entries(matches.iter().copied());

    Ok(Json(SearchResponse {
        query: params.q,
        success: "ok",
        files: map_shop_files(&entries),
        entries,
    }))
}

async fn title_versions(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(title_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<TitleVersions>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;

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
    jar: CookieJar,
    PeerAddr(peer): PeerAddr,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;

    let decoded = percent_decode_str(&path)
        .decode_utf8()
        .map_err(|_| ApiError::InvalidPath)?;
    let sanitized = sanitize_relative_path(&decoded).map_err(map_file_error)?;
    let title = sanitized
        .file_name()
        .and_then(|n: &std::ffi::OsStr| n.to_str())
        .unwrap_or("?")
        .to_string();

    let log_ctx = peer.map(|ip| DownloadLogContext {
        ip,
        title: title.clone(),
    });

    let response = match stream_with_range_support(
        &state.library_root,
        &sanitized,
        &headers,
        log_ctx.as_ref(),
    )
    .await
    {
        Ok(r) => r,
        Err(error) => {
            warn!(path = %sanitized.display(), error = %error, "download failed");
            return Err(map_file_error(error));
        }
    };
    debug!(
        path = %sanitized.display(),
        status = %response.status(),
        "download served"
    );

    Ok(response)
}

async fn download_by_id(
    State(state): State<AppState>,
    jar: CookieJar,
    PeerAddr(peer): PeerAddr,
    Path(id): Path<usize>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;

    let (relative_path, filename) = {
        let catalog = state.catalog.read().await;
        let index = id.checked_sub(1).ok_or(ApiError::NotFound)?;
        let file = catalog.files().get(index).ok_or(ApiError::NotFound)?;
        (file.relative_path.clone(), file.name.clone())
    };

    let log_ctx = peer.map(|ip| DownloadLogContext {
        ip,
        title: filename.clone(),
    });

    let response = match stream_with_range_support(
        &state.library_root,
        &relative_path,
        &headers,
        log_ctx.as_ref(),
    )
    .await
    {
        Ok(r) => r,
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

    debug!(
        file_id = id,
        filename = %filename,
        path = %relative_path.display(),
        status = %response.status(),
        "download by id served"
    );

    Ok(response)
}

async fn shop_icon(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(title_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
    let tid = title_id.trim_end_matches(".png");
    if let Some(info) = state.titledb.lookup(tid).await {
        if let Some(url) = info.icon_url {
            if url.starts_with("http") {
                return Ok(Redirect::temporary(&url).into_response());
            }
        }
    }
    Ok(static_png_response())
}

async fn shop_banner(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(title_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
    let tid = title_id.trim_end_matches(".png");
    if let Some(info) = state.titledb.lookup(tid).await {
        if let Some(url) = info.banner_url {
            if url.starts_with("http") {
                return Ok(Redirect::temporary(&url).into_response());
            }
        }
    }
    Ok(static_png_response())
}

async fn saves_list(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<SavesListResponse>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
    Ok(Json(SavesListResponse {
        success: true,
        saves: Vec::new(),
    }))
}

#[derive(serde::Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login_page(State(state): State<AppState>, jar: CookieJar) -> Result<Response, ApiError> {
    if !state.auth.is_enabled() {
        return Err(ApiError::NotFound);
    }
    if jar
        .get(SESSION_COOKIE)
        .and_then(|c| state.sessions.get(c.value()))
        .is_some()
    {
        return Ok(Redirect::to("/admin").into_response());
    }
    Ok(Html(include_str!("login.html")).into_response())
}

async fn login_post(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> Result<(CookieJar, Redirect), ApiError> {
    if !state.auth.is_enabled() {
        return Err(ApiError::NotFound);
    }
    if !state.auth.is_authorized(&form.username, &form.password) {
        return Ok((jar, Redirect::to("/admin/login?error=1")));
    }
    let token = state.sessions.create(form.username);
    let cookie = Cookie::build((SESSION_COOKIE, token))
        .path("/")
        .http_only(true)
        .same_site(cookie::SameSite::Lax)
        .max_age(cookie::time::Duration::hours(24))
        .build();
    Ok((jar.add(cookie), Redirect::to("/admin")))
}

async fn admin_ui(State(state): State<AppState>, jar: CookieJar) -> Result<Response, ApiError> {
    if !state.auth.is_enabled() {
        return Err(ApiError::NotFound);
    }
    let session_valid = jar
        .get(SESSION_COOKIE)
        .and_then(|c| state.sessions.get(c.value()))
        .is_some();
    if !session_valid {
        return Ok(Redirect::to("/admin/login").into_response());
    }
    Ok(Html(include_str!("admin.html")).into_response())
}

async fn logout(
    State(state): State<AppState>,
    jar: CookieJar,
) -> Result<(CookieJar, Redirect), ApiError> {
    if !state.auth.is_enabled() {
        return Err(ApiError::NotFound);
    }
    if let Some(c) = jar.get(SESSION_COOKIE) {
        state.sessions.remove(c.value());
    }
    Ok((
        jar.remove(Cookie::from(SESSION_COOKIE)),
        Redirect::to("/admin/login"),
    ))
}

async fn settings_ui(State(state): State<AppState>, jar: CookieJar) -> Result<Response, ApiError> {
    if !state.auth.is_enabled() {
        return Err(ApiError::NotFound);
    }
    let session_valid = jar
        .get(SESSION_COOKIE)
        .and_then(|c| state.sessions.get(c.value()))
        .is_some();
    if !session_valid {
        return Ok(Redirect::to("/admin/login").into_response());
    }
    Ok(Html(include_str!("settings.html")).into_response())
}

#[derive(serde::Serialize)]
struct SettingsResponse {
    titledb: TitleDbConfig,
    titledb_entries: usize,
    titledb_last_refresh: Option<String>,
}

#[derive(serde::Deserialize)]
struct SettingsPost {
    titledb: Option<TitleDbConfig>,
}

async fn settings_get(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<SettingsResponse>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
    let titledb = state.titledb.config().await;
    let entries = state.titledb.entry_count().await;
    let last_refresh = state
        .titledb
        .last_refresh()
        .await
        .map(|t| humantime::format_duration(t.elapsed()).to_string());
    Ok(Json(SettingsResponse {
        titledb,
        titledb_entries: entries,
        titledb_last_refresh: last_refresh,
    }))
}

async fn settings_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<SettingsPost>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
    if let Some(titledb) = body.titledb {
        state.titledb.set_config(titledb.clone()).await;
        if let Err(e) = super::settings::save_settings(&state.data_dir, &titledb) {
            tracing::warn!(error = %e, "failed to save settings");
        }
        state.titledb.refresh();
    }
    Ok(Json(serde_json::json!({ "success": true })))
}

async fn titledb_progress_sse(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>> + Send>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
    let rx = state.titledb_progress_tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).map(|r| match r {
        Ok(msg) => Ok(Event::default().data(msg)),
        Err(_) => Ok(Event::default().data("[titledb] (lagged, some messages dropped)")),
    });
    let initial = futures_util::stream::iter([Ok(
        Event::default().data("[titledb] connected, listening for progress...")
    )]);
    let stream = initial.chain(stream);
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

async fn titledb_test_connectivity(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
    let config = state.titledb.config().await;
    let region = &config.region;
    let lang = &config.language;

    let blawar_raw_url =
        format!("https://raw.githubusercontent.com/blawar/titledb/master/{region}.{lang}.json");
    let blawar_jsdelivr_url =
        format!("https://cdn.jsdelivr.net/gh/blawar/titledb@master/{region}.{lang}.json");

    let mut urls: Vec<(&str, String)> = vec![
        ("blawar_raw", blawar_raw_url),
        ("blawar_jsdelivr", blawar_jsdelivr_url),
    ];
    if let Some(u) = &config.url_override {
        urls.push(("url_override", u.clone()));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let mut results = Vec::new();
    for (name, url) in urls {
        let start = std::time::Instant::now();
        match client.head(&url).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let elapsed_ms = start.elapsed().as_millis();
                results.push(serde_json::json!({
                    "source": name,
                    "url": url,
                    "status": status,
                    "ok": (200..400).contains(&status),
                    "elapsed_ms": elapsed_ms,
                }));
            }
            Err(e) => {
                let elapsed_ms = start.elapsed().as_millis();
                results.push(serde_json::json!({
                    "source": name,
                    "url": url,
                    "error": e.to_string(),
                    "ok": false,
                    "elapsed_ms": elapsed_ms,
                }));
            }
        }
    }

    Ok(Json(serde_json::json!({
        "results": results,
        "hint": "If DNS/network errors, try whitelisting: raw.githubusercontent.com, cdn.jsdelivr.net. jsDelivr often bypasses filters."
    })))
}

async fn settings_refresh(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_authorized(&state, &headers, jar.get(SESSION_COOKIE).map(|c| c.value()))?;
    state.titledb.refresh();
    Ok(Json(serde_json::json!({ "success": true })))
}
