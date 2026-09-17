use std::convert::Infallible;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::{FromRequestParts, Multipart, OriginalUri, Path, Query, State};
use axum::http::HeaderMap;
use axum::http::Request;
use axum::http::request::Parts;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use axum_extra::extract::Form;
use axum_extra::extract::cookie::{Cookie, CookieJar};
use futures_util::stream::StreamExt;
use percent_encoding::percent_decode_str;
use tower_governor::{
    GovernorLayer, errors::GovernorError, governor::GovernorConfigBuilder,
    key_extractor::KeyExtractor,
};
use tower_http::compression::CompressionLayer;
use tracing::{debug, warn};

use crate::catalog::{ContentKind, TitleVersions};
use crate::serve_files::{DownloadLogContext, sanitize_relative_path, stream_with_range_support};
use crate::shop::{
    ClientKind, identify_client, is_cyberfoil_request, is_shop_client_request, request_prefers_html,
};

use crate::auth::{AuthRoles, hash_password};
use crate::config::TitleDbConfig;
use crate::scanner::scan_library;
use crate::settings::{SchedulerSettings, ShopSettings, TitleSettings};
use crate::storage::{NewLibrary, NewUser};

use super::auth::{Access, ensure_access, ensure_authorized, extract_basic_auth};
use super::error::ApiError;

const SESSION_COOKIE: &str = "ownfoil_session";
static DOWNLOAD_THROTTLE: LazyLock<dashmap::DashMap<(usize, IpAddr), std::time::Instant>> =
    LazyLock::new(dashmap::DashMap::new);
static THEME_HEAD: LazyLock<String> = LazyLock::new(|| {
    format!(
        "<style>{}</style><script>{}</script>",
        include_str!("theme.css"),
        include_str!("theme.js")
    )
});
static OUTWARD_FACING_IPV4: LazyLock<Option<Ipv4Addr>> = LazyLock::new(|| {
    // Connecting a UDP socket selects the interface used by the default route
    // without sending any traffic. If there is no external route, keep the
    // loopback URL instead of advertising an unusable address.
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
});

/// Extracts peer address from request extensions when available (e.g. from
/// `into_make_service_with_connect_info`). Returns `None` in tests or when
/// connection info is not set.
struct PeerAddr(pub Option<SocketAddr>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientKeyExtractor;

impl KeyExtractor for ClientKeyExtractor {
    type Key = String;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, GovernorError> {
        let ip = forwarded_for_ip(req.headers())
            .or_else(|| x_real_ip(req.headers()))
            .or_else(|| {
                req.extensions()
                    .get::<axum::extract::ConnectInfo<SocketAddr>>()
                    .map(|addr| addr.ip())
            })
            .or_else(|| req.extensions().get::<SocketAddr>().map(SocketAddr::ip))
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        Ok(ip.to_string())
    }
}

fn forwarded_for_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.trim().parse::<IpAddr>().ok())
}

fn x_real_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<IpAddr>().ok())
}

impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let addr = parts.extensions.get::<SocketAddr>().copied().or_else(|| {
            parts.extensions.get::<axum::extract::ConnectInfo<SocketAddr>>().map(|c| c.0)
        });
        std::future::ready(Ok(Self(addr)))
    }
}

use super::responses::{
    CatalogResponse, HealthResponse, SavesListResponse, SearchQuery, SearchResponse,
    SectionsResponse, ShopRootResponse, ShopSectionsQuery, build_catalog_response,
    build_shop_root_files, build_shop_sections_payload, build_upstream_titles_response,
    catalog_sections, map_file_error, map_shop_files, map_to_entries, respond_with_shop_payload,
    static_png_response,
};
use super::state::AppState;

/// Build the Axum router with all routes, layers (rate limit, request ID, trace), and state.
pub fn router(state: AppState) -> Router {
    let governor_conf = GovernorConfigBuilder::default()
        .per_second(20)
        .burst_size(50)
        .key_extractor(ClientKeyExtractor)
        .finish()
        .map(Arc::new);
    if governor_conf.is_none() {
        warn!("governor config invalid; rate limiting disabled");
    }
    let app = Router::new()
        .route("/", get(shop_root))
        .route("/health", get(health))
        .route("/favicon.ico", get(favicon))
        .route("/robots.txt", get(robots))
        .route("/api/catalog", get(catalog_all))
        .route("/api/graphql", get(super::graphql::get).post(super::graphql::post))
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
        .route("/api/titles", get(titles_upstream))
        .route("/api/setup", get(setup_info))
        .route("/api/index", get(catalog_all))
        .route("/api/shop", get(shop_root))
        .route("/api/settings", get(settings_get).post(settings_post))
        .route("/api/settings/titles", post(settings_titles_post))
        .route("/api/settings/shop", post(settings_shop_post))
        .route(
            "/api/settings/library/paths",
            get(settings_library_paths_get)
                .post(settings_library_paths_post)
                .delete(settings_library_paths_delete),
        )
        .route("/api/settings/library/management", post(settings_library_management_post))
        .route("/api/settings/scheduler", post(settings_scheduler_post))
        .route("/api/settings/library/watcher", post(settings_watcher_post))
        .route("/api/settings/worker", post(settings_worker_post))
        .route("/api/upload", post(upload_post))
        .route("/api/library/scan", post(library_scan_post))
        .route("/api/library/organize/preview", post(organizer_preview_post))
        .route("/api/users", get(users_get))
        .route("/api/user", delete(user_delete))
        .route("/api/user/signup", post(user_signup_post))
        .route("/shop", get(shop_root))
        .route("/index", get(catalog_all))
        .route("/titles", get(catalog_all))
        .route("/download/{*path}", get(download))
        .route("/login", get(login_page).post(login_post))
        .route("/logout", get(logout))
        .route("/settings", get(settings_ui))
        .route("/setup", get(setup_page))
        .route("/profile", get(profile_page))
        .route("/admin", get(admin_ui))
        .route("/admin/tasks", get(super::activity::tasks_page))
        .route("/admin/stats", get(super::activity::stats_page))
        .route("/ws/realtime", get(super::activity::websocket))
        .route("/api/ws", get(super::activity::websocket))
        .route("/admin/settings", get(settings_ui))
        .route("/admin/login", get(login_page).post(login_post))
        .route("/admin/logout", get(logout))
        .route("/api/settings/refresh", post(settings_refresh))
        .route("/api/settings/titledb/progress", get(titledb_progress_sse))
        .route("/api/settings/titledb/test", get(titledb_test_connectivity))
        .route("/{*path}", get(shop_root).head(shop_root));

    let app = app
        .layer(tower_http::request_id::SetRequestIdLayer::new(
            axum::http::header::HeaderName::from_static("x-request-id"),
            tower_http::request_id::MakeRequestUuid,
        ))
        .layer(tower_http::request_id::PropagateRequestIdLayer::new(
            axum::http::header::HeaderName::from_static("x-request-id"),
        ))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(CompressionLayer::new())
        .with_state(state);

    if let Some(governor_conf) = governor_conf {
        app.layer(GovernorLayer::new(governor_conf))
    } else {
        app
    }
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let catalog_files = state.catalog.read().await.files().len();
    Json(HealthResponse { status: "ok", catalog_files: Some(catalog_files) })
}

pub(super) fn html_page(template: &str) -> Response {
    Html(template.replace("<!--theme-->", THEME_HEAD.as_str())).into_response()
}

async fn admin_page(state: &AppState) -> Response {
    let files = state.catalog.read().await.files().to_vec();
    let payload = build_shop_sections_payload(&files, 1, &state.titledb).await;
    let icon = payload
        .sections
        .first()
        .and_then(|section| section.items.first())
        .map(|item| item.icon_url.as_str())
        .filter(|url| url.starts_with("https://") || url.starts_with('/'))
        .map_or_else(String::new, |url| {
            let url = url.replace('&', "&amp;").replace('"', "&quot;");
            format!(r#"<link rel="preload" href="{url}" as="image" fetchpriority="high">"#)
        });
    html_page(&include_str!("admin.html").replace("<!--lcp-->", &icon))
}

async fn favicon() -> axum::http::StatusCode {
    axum::http::StatusCode::NO_CONTENT
}

async fn robots() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "User-agent: *\nDisallow:\n",
    )
        .into_response()
}

pub(super) fn session_token(jar: &CookieJar) -> Option<&str> {
    jar.get(SESSION_COOKIE).map(Cookie::value)
}

pub(super) fn ensure_same_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(origin) = headers.get("origin").and_then(|value| value.to_str().ok()) else {
        return Ok(());
    };
    let host = headers.get("host").and_then(|value| value.to_str().ok()).unwrap_or_default();
    let origin_host = origin.split_once("://").map_or(origin, |(_, authority)| authority);
    let origin_host = origin_host.split('/').next().unwrap_or_default();
    if !host.is_empty() && origin_host.eq_ignore_ascii_case(host) {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

async fn shop_root(
    State(state): State<AppState>,
    jar: CookieJar,
    PeerAddr(peer): PeerAddr,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let shop = state.shop.read().await.clone();
    let client = identify_client(&headers);
    if !shop.tinfoil_only_mode
        && client == ClientKind::Browser
        && !is_shop_client_request(&headers)
        && request_prefers_html(&headers)
    {
        if uri.path() != "/" {
            return Ok(Redirect::to("/").into_response());
        }
        let public = state.settings.read().await.shop.public;
        if state.auth.is_enabled() && !public && !session_has_access(&state, &jar, Access::Shop) {
            return Ok(Redirect::to("/login").into_response());
        }
        return Ok(admin_page(&state).await);
    }

    if client != ClientKind::Browser && !client_enabled(&state, client).await {
        return Ok(client_error_response(client, "Shop access from this client is disabled."));
    }

    ensure_authorized(&state, &headers, session_token(&jar)).await?;
    if client == ClientKind::Sphaira {
        return sphaira_response(&state, uri.path(), &headers, peer).await;
    }
    let verified_host = match verify_client_host(&state, client, &headers).await {
        Ok(host) => host,
        Err(response) => return Ok(response),
    };

    let files = {
        let catalog = state.catalog.read().await;
        let filter = uri.path().trim_matches('/').split('/').next().unwrap_or_default();
        build_shop_root_files(&filter_content(catalog.files(), filter))
    };
    debug!(files = files.len(), "shop root requested");
    let payload_shop = shop_for_client(&shop, client);
    respond_with_shop_payload(
        &ShopRootResponse { success: shop.motd.clone(), files, referrer: verified_host },
        &payload_shop,
    )
}

fn shop_for_client(shop: &crate::shop::ShopConfig, client: ClientKind) -> crate::shop::ShopConfig {
    if client == ClientKind::Tinfoil {
        shop.clone()
    } else {
        crate::shop::ShopConfig { encrypt: false, tinfoil_only_mode: false, ..shop.clone() }
    }
}

#[allow(clippy::significant_drop_tightening)]
#[allow(clippy::result_large_err)] // Return the ready HTTP response without an extra allocation.
async fn verify_client_host(
    state: &AppState,
    client: ClientKind,
    headers: &HeaderMap,
) -> Result<Option<String>, Response> {
    if !matches!(client, ClientKind::Tinfoil | ClientKind::CyberFoil) {
        return Ok(None);
    }
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("https"));
    if !secure {
        return Ok(None);
    }
    let request_host =
        headers.get("host").and_then(|value| value.to_str().ok()).unwrap_or_default();
    let request_hauth =
        headers.get("hauth").and_then(|value| value.to_str().ok()).unwrap_or_default();
    let mut settings = state.settings.write().await;
    let configured_host = settings.shop.host.clone();
    if configured_host.is_empty() {
        return Ok(None);
    }
    if request_host != configured_host {
        return Err(client_error_response(
            client,
            &format!("Incorrect URL referrer detected: {request_host}."),
        ));
    }
    let stored = match client {
        ClientKind::Tinfoil => settings.shop.clients.tinfoil.hauth.get(request_host).cloned(),
        ClientKind::CyberFoil => settings.shop.clients.cyberfoil.hauth.get(request_host).cloned(),
        ClientKind::Browser | ClientKind::Sphaira => None,
    };
    if let Some(stored) = stored {
        if stored != request_hauth {
            return Err(client_error_response(
                client,
                &format!("Incorrect Hauth for URL `{request_host}`."),
            ));
        }
        return Ok(Some(format!("https://{configured_host}")));
    }

    let admin = extract_basic_auth(headers)
        .and_then(|(username, password)| {
            state
                .auth
                .is_authorized(&username, &password)
                .then(|| state.auth.roles(&username))
                .flatten()
        })
        .is_some_and(|roles| roles.admin_access);
    if admin && !request_hauth.is_empty() {
        match client {
            ClientKind::Tinfoil => {
                settings
                    .shop
                    .clients
                    .tinfoil
                    .hauth
                    .insert(request_host.to_string(), request_hauth.to_string());
            }
            ClientKind::CyberFoil => {
                settings
                    .shop
                    .clients
                    .cyberfoil
                    .hauth
                    .insert(request_host.to_string(), request_hauth.to_string());
            }
            ClientKind::Browser | ClientKind::Sphaira => {}
        }
        if settings.save(&state.settings_path).is_err() {
            return Err(client_error_response(client, "Failed to persist Hauth."));
        }
        return Ok(Some(format!("https://{request_host}")));
    }
    Ok(None)
}

async fn client_enabled(state: &AppState, client: ClientKind) -> bool {
    let settings = state.settings.read().await;
    match client {
        ClientKind::Browser => true,
        ClientKind::Tinfoil => settings.shop.clients.tinfoil.enabled,
        ClientKind::CyberFoil => settings.shop.clients.cyberfoil.enabled,
        ClientKind::Sphaira => settings.shop.clients.sphaira.enabled,
    }
}

fn client_error_response(client: ClientKind, message: &str) -> Response {
    if client == ClientKind::Sphaira {
        Html(format!(
            "<!DOCTYPE html><html><body><table><a href=\"00 - ERROR\"> </a>\n<a href=\"01 - {}\"> </a></table></body></html>",
            html_escape(message)
        ))
        .into_response()
    } else {
        Json(serde_json::json!({ "error": message })).into_response()
    }
}

fn filter_content(
    files: &[crate::catalog::ContentFile],
    filter: &str,
) -> Vec<crate::catalog::ContentFile> {
    files
        .iter()
        .filter(|file| match filter {
            "base" => !file.is_multicontent() && file.contains_kind(ContentKind::Base),
            "update" => !file.is_multicontent() && file.contains_kind(ContentKind::Update),
            "dlc" => !file.is_multicontent() && file.contains_kind(ContentKind::Dlc),
            "multi" => file.is_multicontent(),
            _ => true,
        })
        .cloned()
        .collect()
}

async fn sphaira_response(
    state: &AppState,
    request_path: &str,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Result<Response, ApiError> {
    let decoded = percent_decode_str(request_path.trim_matches('/'))
        .decode_utf8()
        .map_err(|_| ApiError::InvalidPath)?;
    let mut parts = decoded.split('/');
    let first = parts.next().unwrap_or_default();
    let filter = matches!(first, "base" | "update" | "dlc" | "multi").then_some(first);
    let virtual_path =
        if filter.is_some() { parts.collect::<Vec<_>>().join("/") } else { decoded.to_string() };
    let files = {
        let catalog = state.catalog.read().await;
        filter_content(catalog.files(), filter.unwrap_or_default())
    };

    if let Some(file) = files.iter().find(|file| {
        file.name == virtual_path.rsplit('/').next().unwrap_or_default()
            && crate::scanner::is_supported_content(&file.relative_path)
    }) {
        let root = if file.library_root.as_os_str().is_empty() {
            &state.library_root
        } else {
            &file.library_root
        };
        let log = peer.map(|ip| DownloadLogContext { ip, title: file.name.clone() });
        let response = stream_with_range_support(root, &file.relative_path, headers, log.as_ref())
            .await
            .map_err(|error| map_file_error(&error))?;
        increment_download_throttled(state, file.id, peer).await;
        return Ok(response);
    }

    let mut items = std::collections::BTreeSet::new();
    for file in files {
        let relative = file.relative_path.to_string_lossy().replace('\\', "/");
        let remainder = if virtual_path.is_empty() {
            relative.as_str()
        } else if let Some(remainder) = relative.strip_prefix(&format!("{virtual_path}/")) {
            remainder
        } else {
            continue;
        };
        if let Some((directory, _)) = remainder.split_once('/') {
            items.insert(format!("{directory}/"));
        } else if !remainder.is_empty() {
            items.insert(remainder.to_string());
        }
    }
    let mut items = items.into_iter().collect::<Vec<_>>();
    items.sort_by(|left, right| {
        (!left.ends_with('/'), left.to_ascii_lowercase())
            .cmp(&(!right.ends_with('/'), right.to_ascii_lowercase()))
    });
    let content = if items.is_empty() {
        "<a href=\"No content available\"> </a>".to_string()
    } else {
        items
            .into_iter()
            .map(|item| format!("<a href=\"{}\"> </a>", html_escape(&item)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(Html(format!("<!DOCTYPE html><html><body><table>{content}</table></body></html>"))
        .into_response())
}

fn html_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

async fn catalog_all(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<CatalogResponse>, ApiError> {
    ensure_authorized(&state, &headers, session_token(&jar)).await?;
    let entries = {
        let catalog = state.catalog.read().await;
        map_to_entries(catalog.files())
    };
    debug!(entries = entries.len(), "catalog requested");
    Ok(Json(build_catalog_response(entries)))
}

async fn titles_upstream(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_authorized(&state, &headers, session_token(&jar)).await?;
    let files = state.catalog.read().await.files().to_vec();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for file in &files {
        file.id.hash(&mut hasher);
        file.library_root.hash(&mut hasher);
        file.relative_path.hash(&mut hasher);
        file.size.hash(&mut hasher);
        file.title_id.hash(&mut hasher);
        file.version.hash(&mut hasher);
        file.kind.hash(&mut hasher);
        file.identified_contents.hash(&mut hasher);
    }
    state.titledb.generation().hash(&mut hasher);
    let cache_key = hasher.finish();
    if let Some((_, cached)) =
        state.titles_cache.read().await.as_ref().filter(|(key, _)| *key == cache_key)
    {
        return Ok(Json(cached.clone()));
    }
    let response = build_upstream_titles_response(&files, &state.titledb).await;
    *state.titles_cache.write().await = Some((cache_key, response.clone()));
    Ok(Json(response))
}

async fn setup_info(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_authorized(&state, &headers, session_token(&jar)).await?;
    let settings = state.settings.read().await;
    let host = settings.shop.host.clone();
    let public = settings.shop.public;
    let tinfoil = settings.shop.clients.tinfoil.enabled;
    let cyberfoil = settings.shop.clients.cyberfoil.enabled;
    let sphaira = settings.shop.clients.sphaira.enabled;
    drop(settings);
    Ok(Json(serde_json::json!({
        "host": host,
        "external_ip": *OUTWARD_FACING_IPV4,
        "public": public,
        "clients": {
            "tinfoil": tinfoil,
            "cyberfoil": cyberfoil,
            "sphaira": sphaira
        },
        "filters": ["base", "update", "dlc", "multi"]
    })))
}

async fn sections(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<SectionsResponse>, ApiError> {
    ensure_authorized(&state, &headers, session_token(&jar)).await?;
    debug!("sections requested");
    Ok(Json(SectionsResponse { sections: catalog_sections() }))
}

async fn shop_sections(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<ShopSectionsQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers, session_token(&jar)).await?;

    let (files, limit) = {
        let catalog = state.catalog.read().await;
        let limit = if is_cyberfoil_request(&headers) {
            catalog.files().len().max(1)
        } else {
            query.limit.unwrap_or(50).max(1)
        };
        (catalog.files().to_vec(), limit)
    };
    let payload = build_shop_sections_payload(&files, limit, &state.titledb).await;
    debug!(limit, sections = payload.sections.len(), "shop sections requested");
    let shop = state.shop.read().await.clone();
    let client = identify_client(&headers);
    respond_with_shop_payload(&payload, &shop_for_client(&shop, client))
}

async fn section_entries(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(section): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CatalogResponse>, ApiError> {
    ensure_authorized(&state, &headers, session_token(&jar)).await?;

    let entries = {
        let catalog = state.catalog.read().await;
        match section.as_str() {
            "all" | "new" | "recommended" => map_to_entries(catalog.files()),
            "base" | "games" => map_to_entries(catalog.files_by_kind(ContentKind::Base)),
            "updates" | "update" => map_to_entries(catalog.files_by_kind(ContentKind::Update)),
            "dlc" => map_to_entries(catalog.files_by_kind(ContentKind::Dlc)),
            _ => Vec::new(),
        }
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
    ensure_authorized(&state, &headers, session_token(&jar)).await?;

    let matches = {
        let catalog = state.catalog.read().await;
        map_to_entries(catalog.search(&params.q).iter().copied())
    };
    debug!(query = %params.q, results = matches.len(), "search requested");
    let entries = matches;

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
    ensure_authorized(&state, &headers, session_token(&jar)).await?;

    let versions = {
        let catalog = state.catalog.read().await;
        catalog.versions(&title_id)
    }
    .ok_or(ApiError::TitleNotFound)?;
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
    ensure_authorized(&state, &headers, session_token(&jar)).await?;

    let decoded = percent_decode_str(&path).decode_utf8().map_err(|_| ApiError::InvalidPath)?;
    let sanitized = sanitize_relative_path(&decoded).map_err(|error| map_file_error(&error))?;
    let title =
        sanitized.file_name().and_then(|n: &std::ffi::OsStr| n.to_str()).unwrap_or("?").to_string();

    let log_ctx = peer.map(|ip| DownloadLogContext { ip, title: title.clone() });

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
            return Err(map_file_error(&error));
        }
    };
    debug!(
        path = %sanitized.display(),
        status = %response.status(),
        "download served"
    );

    let downloaded_id = state
        .catalog
        .read()
        .await
        .files()
        .iter()
        .find(|file| file.relative_path == sanitized)
        .map(|file| file.id);
    if let Some(id) = downloaded_id {
        increment_download_throttled(&state, id, peer).await;
    }

    Ok(response)
}

async fn download_by_id(
    State(state): State<AppState>,
    jar: CookieJar,
    PeerAddr(peer): PeerAddr,
    Path(id): Path<usize>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers, session_token(&jar)).await?;

    let file = {
        let catalog = state.catalog.read().await;
        catalog
            .files()
            .iter()
            .find(|file| file.id == id && file.id != 0)
            .or_else(|| id.checked_sub(1).and_then(|index| catalog.files().get(index)))
            .cloned()
            .ok_or(ApiError::NotFound)?
    };
    let library_root = if file.library_root.as_os_str().is_empty() {
        &state.library_root
    } else {
        &file.library_root
    };
    let relative_path = file.relative_path;
    let filename = file.name;

    let log_ctx = peer.map(|ip| DownloadLogContext { ip, title: filename.clone() });

    let response =
        match stream_with_range_support(library_root, &relative_path, &headers, log_ctx.as_ref())
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
                return Err(map_file_error(&error));
            }
        };

    increment_download_throttled(&state, id, peer).await;

    debug!(
        file_id = id,
        filename = %filename,
        path = %relative_path.display(),
        status = %response.status(),
        "download by id served"
    );

    Ok(response)
}

async fn increment_download_throttled(state: &AppState, id: usize, peer: Option<SocketAddr>) {
    if id == 0 {
        return;
    }
    if let Some(peer) = peer {
        let key = (id, peer.ip());
        let now = std::time::Instant::now();
        if DOWNLOAD_THROTTLE
            .get(&key)
            .is_some_and(|last| now.duration_since(*last) < Duration::from_mins(1))
        {
            return;
        }
        DOWNLOAD_THROTTLE.insert(key, now);
        DOWNLOAD_THROTTLE
            .retain(|_, instant| now.duration_since(*instant) < Duration::from_mins(1));
    }
    if let Some(storage) = &state.storage {
        let _ = storage.increment_file_download_count(i64::try_from(id).unwrap_or(i64::MAX)).await;
    }
}

async fn shop_icon(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(title_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers, session_token(&jar)).await?;
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
    ensure_authorized(&state, &headers, session_token(&jar)).await?;
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
    ensure_access(&state, &headers, session_token(&jar), Access::Backup).await?;
    Ok(Json(SavesListResponse { success: true, saves: Vec::new() }))
}

#[derive(serde::Deserialize)]
struct LoginForm {
    #[serde(alias = "user")]
    username: String,
    password: String,
    next: Option<String>,
    remember: Option<String>,
}

#[derive(serde::Deserialize)]
struct LoginQuery {
    next: Option<String>,
}

fn local_next(next: Option<&str>) -> Option<&str> {
    let next = next?;
    let decoded = percent_decode_str(next).decode_utf8().ok()?;
    if [next, decoded.as_ref()].iter().any(|value| {
        !value.starts_with('/')
            || value.starts_with("//")
            || value.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    }) || decoded.contains('%')
        || next.parse::<axum::http::Uri>().is_err()
    {
        return None;
    }
    Some(next)
}

async fn login_page(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<LoginQuery>,
) -> Result<Response, ApiError> {
    let next = local_next(query.next.as_deref());
    if jar.get(SESSION_COOKIE).and_then(|c| state.sessions.get(c.value())).is_some() {
        return Ok(Redirect::to(next.unwrap_or("/admin")).into_response());
    }
    let template = include_str!("login.html").replace(
        "</form>",
        &format!(
            "<input type=\"hidden\" name=\"next\" value=\"{}\"></form>",
            html_escape(next.unwrap_or_default())
        ),
    );
    Ok(html_page(&template))
}

async fn login_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Result<(CookieJar, Redirect), ApiError> {
    ensure_same_origin(&headers)?;
    if !state.auth.is_authorized(&form.username, &form.password) {
        return Ok((jar, Redirect::to("/admin/login?error=1")));
    }
    let redirect = state.auth.roles(&form.username).map_or("/setup", |roles| {
        if roles.admin_access {
            "/admin"
        } else if roles.backup_access {
            "/profile"
        } else {
            "/setup"
        }
    });
    let redirect = local_next(form.next.as_deref()).unwrap_or(redirect);
    let token = state.sessions.create(form.username);
    let mut cookie = Cookie::build((SESSION_COOKIE, token))
        .path("/")
        .http_only(true)
        .secure(!state.insecure_admin_cookie)
        .same_site(cookie::SameSite::Lax);
    if form.remember.as_deref().is_some_and(|value| !value.is_empty()) {
        cookie = cookie.max_age(cookie::time::Duration::hours(24));
    }
    Ok((jar.add(cookie.build()), Redirect::to(redirect)))
}

fn session_has_access(state: &AppState, jar: &CookieJar, access: Access) -> bool {
    let Some(username) = jar.get(SESSION_COOKIE).and_then(|c| state.sessions.get(c.value())) else {
        return false;
    };
    state.auth.roles(&username).is_some_and(|roles| match access {
        Access::Admin => roles.admin_access,
        Access::Shop => roles.shop_access || roles.admin_access,
        Access::Backup => roles.backup_access || roles.admin_access,
    })
}

async fn admin_ui(State(state): State<AppState>, jar: CookieJar) -> Result<Response, ApiError> {
    if state.auth.is_enabled() && !session_has_access(&state, &jar, Access::Admin) {
        return Ok(Redirect::to("/admin/login").into_response());
    }
    Ok(admin_page(&state).await)
}

async fn logout(
    State(state): State<AppState>,
    jar: CookieJar,
) -> Result<(CookieJar, Redirect), ApiError> {
    if let Some(c) = jar.get(SESSION_COOKIE) {
        state.sessions.remove(c.value());
    }
    Ok((jar.remove(Cookie::from(SESSION_COOKIE)), Redirect::to("/admin/login")))
}

async fn settings_ui(State(state): State<AppState>, jar: CookieJar) -> Result<Response, ApiError> {
    if state.auth.is_enabled() && !session_has_access(&state, &jar, Access::Admin) {
        return Ok(Redirect::to("/admin/login").into_response());
    }
    Ok(html_page(include_str!("settings.html")))
}

async fn setup_page(State(state): State<AppState>, jar: CookieJar) -> Result<Response, ApiError> {
    let public = state.settings.read().await.shop.public;
    if state.auth.is_enabled() && !public && !session_has_access(&state, &jar, Access::Shop) {
        return Ok(Redirect::to("/login").into_response());
    }
    Ok(html_page(include_str!("setup.html")))
}

async fn profile_page(State(state): State<AppState>, jar: CookieJar) -> Result<Response, ApiError> {
    if state.auth.is_enabled() && !session_has_access(&state, &jar, Access::Backup) {
        return Ok(Redirect::to("/login").into_response());
    }
    Ok(html_page(include_str!("profile.html")))
}

#[derive(serde::Deserialize)]
struct SettingsPost {
    titledb: Option<TitleDbConfig>,
}

async fn settings_get(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    let mut settings = serde_json::to_value(state.settings.read().await.redacted())
        .map_err(|_| ApiError::Internal)?;
    let key_status = crate::keys::inspect(&state.keys_path);
    if let Some(titles) = settings.get_mut("titles").and_then(serde_json::Value::as_object_mut) {
        titles.insert("valid_keys".to_string(), serde_json::json!(key_status.valid_keys));
        titles.insert("missing_keys".to_string(), serde_json::json!(key_status.missing_keys));
        titles.insert("corrupt_keys".to_string(), serde_json::json!(key_status.corrupt_keys));
    }
    Ok(Json(settings))
}

async fn settings_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<SettingsPost>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    if let Some(titledb) = body.titledb {
        state.titledb.set_config(titledb.clone()).await;
        if let Err(e) = super::settings::save_settings(&state.data_dir, &titledb) {
            tracing::warn!(error = %e, "failed to save settings");
        }
        state.titledb.refresh();
    }
    Ok(Json(serde_json::json!({ "success": true })))
}

async fn settings_titles_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<TitleSettings>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    if state.titledb.region_language_available(&body.region, &body.language).await == Some(false) {
        return Ok(Json(serde_json::json!({
            "success": false,
            "errors": [{
                "path": "titles",
                "error": format!("The region/language pair {}/{} is not available.", body.region, body.language)
            }]
        })));
    }
    {
        let mut settings = state.settings.write().await;
        settings.titles = body.clone();
        settings.save(&state.settings_path).map_err(|error| {
            warn!(error = %error, "failed to save title settings");
            ApiError::Internal
        })?;
    }
    let mut titledb = state.titledb.config().await;
    titledb.region = body.region;
    titledb.language = body.language;
    state.titledb.set_config(titledb).await;
    state.titledb.refresh();
    Ok(Json(serde_json::json!({ "success": true, "errors": [] })))
}

async fn settings_shop_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    let mut settings = state.settings.write().await;
    let mut value = serde_json::to_value(&settings.shop).map_err(|_| ApiError::Internal)?;
    let merged = merge_shop_patch(&mut value, &body)
        .map_err(str::to_string)
        .and_then(|()| serde_json::from_value::<ShopSettings>(value).map_err(|e| e.to_string()));
    let mut candidate = settings.clone();
    candidate.shop = match merged {
        Ok(shop) => shop,
        Err(error) => {
            return Ok(Json(serde_json::json!({
                "success": false, "errors": [{"path": "shop", "error": error}]
            })));
        }
    };
    if let Some((_, host)) = candidate.shop.host.split_once("://") {
        candidate.shop.host = host.to_string();
    }
    candidate.save(&state.settings_path).map_err(|error| {
        warn!(error = %error, "failed to save shop settings");
        ApiError::Internal
    })?;
    let mut shop = state.shop.write().await;
    shop.motd.clone_from(&candidate.shop.motd);
    shop.encrypt = candidate.shop.clients.tinfoil.encrypt;
    *settings = candidate;
    drop(shop);
    drop(settings);
    Ok(Json(serde_json::json!({ "success": true, "errors": [] })))
}

fn merge_shop_patch(
    target: &mut serde_json::Value,
    patch: &serde_json::Value,
) -> Result<(), &'static str> {
    if let Some(target) = target.as_object_mut() {
        let patch = patch.as_object().ok_or("Expected a shop settings object")?;
        for (key, value) in patch {
            if matches!(key.as_str(), "hauth" | "clientCertKey") {
                continue;
            }
            if let Some(target) = target.get_mut(key) {
                merge_shop_patch(target, value)?;
            }
        }
    } else {
        target.clone_from(patch);
    }
    Ok(())
}

async fn settings_library_paths_get(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    let paths = state.settings.read().await.library.paths.clone();
    Ok(Json(
        serde_json::json!({ "success": true, "errors": [], "paths": paths, "watcher": state.settings.read().await.library.watcher }),
    ))
}

#[derive(serde::Deserialize)]
struct LibraryPathRequest {
    path: std::path::PathBuf,
}

async fn settings_library_paths_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<LibraryPathRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    if !body.path.is_dir() {
        return Ok(Json(serde_json::json!({
            "success": false,
            "errors": [{"path": "library/paths", "error": "Path does not exist or is not a directory."}]
        })));
    }
    {
        let mut settings = state.settings.write().await;
        if settings.library.paths.contains(&body.path) {
            return Ok(Json(serde_json::json!({
                "success": false,
                "errors": [{"path": "library/paths", "error": format!("Path {} already configured.", body.path.display())}]
            })));
        }
        settings.library.paths.push(body.path.clone());
        settings.save(&state.settings_path).map_err(|_| ApiError::Internal)?;
    }
    if let Some(storage) = &state.storage {
        storage
            .upsert_library(NewLibrary {
                path: body.path.to_string_lossy().into_owned(),
                last_scan: None,
            })
            .await
            .map_err(|_| ApiError::Internal)?;
    }
    if let Some(storage) = &state.storage {
        crate::tasks::enqueue(
            storage,
            "scan_library",
            serde_json::json!({"library_path":body.path}),
        )
        .await
        .map_err(|_| ApiError::Internal)?;
    }
    Ok(Json(serde_json::json!({ "success": true, "errors": [] })))
}

async fn settings_library_paths_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<LibraryPathRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    {
        let mut settings = state.settings.write().await;
        if !settings.library.paths.contains(&body.path) {
            return Ok(Json(serde_json::json!({
                "success": false,
                "errors": [{"path": "library/paths", "error": format!("Path {} not configured.", body.path.display())}]
            })));
        }
        if settings.library.paths.len() <= 1 && settings.library.paths.contains(&body.path) {
            return Ok(Json(serde_json::json!({
                "success": false,
                "errors": [{"path": "library/paths", "error": "At least one library path is required."}]
            })));
        }
        settings.library.paths.retain(|path| path != &body.path);
        settings.save(&state.settings_path).map_err(|_| ApiError::Internal)?;
    }
    if let Some(storage) = &state.storage {
        if let Some(library) = storage
            .get_library_by_path(body.path.to_string_lossy().into_owned())
            .await
            .map_err(|_| ApiError::Internal)?
        {
            storage.delete_library(library.id).await.map_err(|_| ApiError::Internal)?;
        }
    }
    let remaining = state
        .catalog
        .read()
        .await
        .files()
        .iter()
        .filter(|file| file.library_root != body.path)
        .cloned()
        .collect();
    *state.catalog.write().await = crate::catalog::Catalog::from_files(remaining);
    Ok(Json(serde_json::json!({ "success": true, "errors": [] })))
}

async fn settings_watcher_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    update_settings_section(&state, &jar, &headers, "library/watcher", body).await
}

async fn settings_worker_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    update_settings_section(&state, &jar, &headers, "worker", body).await
}

fn merge(target: &mut serde_json::Value, patch: serde_json::Value) {
    if let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            merge(target.entry(key).or_insert(serde_json::Value::Null), value.clone());
        }
    } else {
        *target = patch;
    }
}
async fn update_settings_section(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    section: &str,
    body: serde_json::Value,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(state, headers, session_token(jar), Access::Admin).await?;
    ensure_same_origin(headers)?;
    let mut settings = state.settings.write().await;
    let mut value = serde_json::to_value(&*settings).map_err(|_| ApiError::Internal)?;
    let pointer = format!("/{section}");
    let target = value.pointer_mut(&pointer).ok_or(ApiError::Internal)?;
    merge(target, body);
    let candidate = serde_json::from_value::<crate::settings::Settings>(value);
    let candidate = match candidate {
        Ok(candidate) => candidate,
        Err(error) => {
            return Ok(Json(
                serde_json::json!({"success": false, "errors": [{"path": section, "error": error.to_string()}]}),
            ));
        }
    };
    if let Err(error) = candidate.validate() {
        return Ok(Json(
            serde_json::json!({"success": false, "errors": [{"path": section, "error": error.to_string()}]}),
        ));
    }
    candidate.save(&state.settings_path).map_err(|_| ApiError::Internal)?;
    *settings = candidate;
    drop(settings);
    Ok(Json(serde_json::json!({"success": true, "errors": []})))
}

async fn settings_library_management_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let result =
        update_settings_section(&state, &jar, &headers, "library/management", body).await?;
    if result.0["success"] == true {
        if let Some(storage) = &state.storage {
            crate::tasks::enqueue(storage, "process_library", serde_json::json!({}))
                .await
                .map_err(|_| ApiError::Internal)?;
        }
    }
    Ok(result)
}

async fn settings_scheduler_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<SchedulerSettings>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    if let Err(error) = crate::settings::validate_interval(&body.scan_interval) {
        return Ok(Json(serde_json::json!({
            "success": false,
            "errors": [{"path": "scheduler/titledb_update_interval", "error": error.to_string()}]
        })));
    }
    let mut settings = state.settings.write().await;
    settings.scheduler = body;
    settings.save(&state.settings_path).map_err(|_| ApiError::Internal)?;
    drop(settings);
    Ok(Json(serde_json::json!({ "success": true, "errors": [] })))
}

#[derive(serde::Deserialize)]
struct ScanRequest {
    path: Option<std::path::PathBuf>,
}

async fn organizer_preview_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    let management = state.settings.read().await.library.management.clone();
    let catalog = state.catalog.read().await;
    Ok(Json(serde_json::json!({
        "success": true,
        "moves": crate::organizer::preview(catalog.files(), &management)
    })))
}

async fn library_scan_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<ScanRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    let Ok(_scan_guard) = state.scan_lock.try_lock() else {
        return Ok(Json(serde_json::json!({ "success": false, "errors": [] })));
    };
    let roots = state.settings.read().await.library.paths.clone();
    if body.path.as_ref().is_some_and(|path| !roots.contains(path)) {
        return Ok(Json(serde_json::json!({
            "success": false,
            "errors": ["Unknown library path"]
        })));
    }
    let storage = state.storage.as_ref().ok_or(ApiError::Internal)?;
    let management = state.settings.read().await.library.management.clone();
    let mut files = Vec::new();
    let requested_path = body.path.clone();
    let selected = body.path.map_or(roots, |path| vec![path]);
    for root in selected {
        let mut scanned = scan_library(&root).await.map_err(|_| ApiError::Internal)?;
        scanned = crate::identifier::identify_files(&root, scanned, &state.keys_path).await;
        if crate::organizer::organize(&root, &scanned, &management)
            .await
            .map_err(|_| ApiError::Internal)?
        {
            scanned = scan_library(&root).await.map_err(|_| ApiError::Internal)?;
            scanned = crate::identifier::identify_files(&root, scanned, &state.keys_path).await;
        }
        let persisted =
            storage.reconcile_library_scan(root, scanned).await.map_err(|_| ApiError::Internal)?;
        files.extend(persisted);
    }
    if let Some(path) = requested_path {
        let mut merged = state
            .catalog
            .read()
            .await
            .files()
            .iter()
            .filter(|file| file.library_root != path)
            .cloned()
            .collect::<Vec<_>>();
        merged.extend(files);
        *state.catalog.write().await = crate::catalog::Catalog::from_files(merged);
    } else {
        *state.catalog.write().await = crate::catalog::Catalog::from_files(files);
    }
    Ok(Json(serde_json::json!({ "success": true, "errors": [] })))
}

async fn upload_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, ApiError> {
    const MAX_KEYS_SIZE: usize = 1024 * 1024;
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    while let Some(field) = multipart.next_field().await.map_err(|_| ApiError::InvalidPath)? {
        if field.name() != Some("file") {
            continue;
        }
        let filename = field.file_name().unwrap_or_default();
        let valid_extension = std::path::Path::new(filename)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("txt") || extension.eq_ignore_ascii_case("keys")
            });
        if !valid_extension {
            return Ok(Json(serde_json::json!({
                "success": false,
                "errors": ["Unsupported keys file extension"],
                "data": {}
            })));
        }
        let bytes = field.bytes().await.map_err(|_| ApiError::InvalidPath)?;
        if bytes.is_empty() || bytes.len() > MAX_KEYS_SIZE {
            return Ok(Json(serde_json::json!({
                "success": false,
                "errors": ["Keys file is empty or too large"],
                "data": {}
            })));
        }
        let status = crate::keys::inspect_bytes(&bytes);
        let temp_path = state.keys_path.with_extension("txt.tmp");
        tokio::fs::write(&temp_path, &bytes).await.map_err(|_| ApiError::Internal)?;
        tokio::fs::rename(&temp_path, &state.keys_path).await.map_err(|_| ApiError::Internal)?;
        return Ok(Json(serde_json::json!({
            "success": true,
            "errors": [],
            "data": {
                "valid_keys": status.valid_keys,
                "missing_keys": status.missing_keys,
                "corrupt_keys": status.corrupt_keys
            }
        })));
    }
    Ok(Json(serde_json::json!({
        "success": false,
        "errors": ["Missing multipart file field"],
        "data": {}
    })))
}

async fn users_get(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    let users = if let Some(storage) = &state.storage {
        storage
            .list_users()
            .await
            .map_err(|_| ApiError::Internal)?
            .into_iter()
            .map(|user| {
                serde_json::json!({
                    "id": user.id,
                    "user": user.username,
                    "admin_access": user.can_admin,
                    "shop_access": user.can_download,
                    "backup_access": user.can_upload
                })
            })
            .collect::<Vec<_>>()
    } else {
        state
            .auth
            .usernames()
            .into_iter()
            .enumerate()
            .map(|(index, username)| {
                let roles = state.auth.roles(&username).unwrap_or(AuthRoles {
                    admin_access: true,
                    shop_access: true,
                    backup_access: true,
                });
                serde_json::json!({
                    "id": index + 1,
                    "user": username,
                    "admin_access": roles.admin_access,
                    "shop_access": roles.shop_access,
                    "backup_access": roles.backup_access
                })
            })
            .collect::<Vec<_>>()
    };
    Ok(Json(serde_json::Value::Array(users)))
}

#[derive(serde::Deserialize)]
struct DeleteUserRequest {
    user_id: i64,
}

async fn user_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<DeleteUserRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    let storage = state.storage.as_ref().ok_or(ApiError::Internal)?;
    let user = storage.get_user(body.user_id).await.map_err(|_| ApiError::Internal)?;
    let deleted = storage.delete_user(body.user_id).await.map_err(|_| ApiError::Internal)?;
    if let Some(user) = user {
        state.auth.remove_user(&user.username);
    }
    Ok(Json(serde_json::json!({ "success": deleted })))
}

#[derive(serde::Deserialize)]
struct SignupRequest {
    user: String,
    password: String,
    admin_access: bool,
    #[serde(default)]
    shop_access: bool,
    #[serde(default)]
    backup_access: bool,
}

async fn user_signup_post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<SignupRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    if let Some(error) = validate_username(&body.user) {
        return Ok(Json(serde_json::json!({
            "success": false,
            "error": format!("Username validation failed: {error}")
        })));
    }
    if let Some(error) = validate_password(&body.password) {
        return Ok(Json(serde_json::json!({
            "success": false,
            "error": format!("Password validation failed: {error}")
        })));
    }
    let storage = state.storage.as_ref().ok_or(ApiError::Internal)?;
    if storage
        .get_user_by_username(body.user.clone())
        .await
        .map_err(|_| ApiError::Internal)?
        .is_some()
    {
        return Ok(Json(serde_json::json!({ "success": false, "error": "User already exists" })));
    }
    let existing_users = storage.list_users().await.map_err(|_| ApiError::Internal)?;
    if existing_users.is_empty() && !body.admin_access {
        return Ok(Json(serde_json::json!({
            "success": false,
            "status_code": 400,
            "location": "/settings"
        })));
    }
    let roles = if body.admin_access {
        AuthRoles { admin_access: true, shop_access: true, backup_access: true }
    } else {
        AuthRoles {
            admin_access: false,
            shop_access: body.shop_access,
            backup_access: body.backup_access,
        }
    };
    let password_hash = hash_password(&body.password).map_err(|_| ApiError::Internal)?;
    storage
        .upsert_user(NewUser {
            username: body.user.clone(),
            password_hash: password_hash.clone(),
            can_admin: roles.admin_access,
            can_upload: roles.backup_access,
            can_download: roles.shop_access,
            enabled: true,
        })
        .await
        .map_err(|_| ApiError::Internal)?;
    state.auth.upsert_hashed_user(body.user, password_hash, roles);
    let mut response = serde_json::json!({ "success": true });
    if existing_users.is_empty() {
        response["status_code"] = serde_json::json!(302);
        response["location"] = serde_json::json!("/settings");
    }
    Ok(Json(response))
}

fn validate_username(username: &str) -> Option<&'static str> {
    if username.is_empty() {
        return Some("Username cannot be empty");
    }
    if username.contains(':') {
        return Some("Username cannot contain colons (:)");
    }
    if username.chars().any(char::is_control) {
        return Some("Username contains invalid control characters");
    }
    None
}

fn validate_password(password: &str) -> Option<&'static str> {
    if password.is_empty() {
        return Some("Password cannot be empty");
    }
    if password.chars().any(char::is_control) {
        return Some("Password contains invalid control characters");
    }
    if password.chars().any(|character| matches!(character, '@' | '&' | '/' | '?' | '#' | '=')) {
        return Some("Password contains invalid characters. Please avoid: @ & / ? # =");
    }
    None
}

async fn titledb_progress_sse(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>> + Send>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    let rx = state.titledb_progress_tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).map(|result| {
        result.map_or_else(
            |_| Ok(Event::default().data("[titledb] (lagged, some messages dropped)")),
            |msg| Ok(Event::default().data(msg)),
        )
    });
    let initial = futures_util::stream::iter([Ok(
        Event::default().data("[titledb] connected, listening for progress...")
    )]);
    let stream = initial.chain(stream);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("ping")))
}

async fn titledb_test_connectivity(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    let config = state.titledb.config().await;
    let region = &config.region;
    let lang = &config.language;

    let blawar_raw_url =
        format!("https://raw.githubusercontent.com/blawar/titledb/master/{region}.{lang}.json");
    let blawar_jsdelivr_url =
        format!("https://cdn.jsdelivr.net/gh/blawar/titledb@master/{region}.{lang}.json");

    let mut urls: Vec<(&str, String)> =
        vec![("blawar_raw", blawar_raw_url), ("blawar_jsdelivr", blawar_jsdelivr_url)];
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
    ensure_access(&state, &headers, session_token(&jar), Access::Admin).await?;
    ensure_same_origin(&headers)?;
    state.titledb.refresh();
    Ok(Json(serde_json::json!({ "success": true })))
}
