use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use percent_encoding::percent_decode_str;
use tower_governor::{
    governor::GovernorConfigBuilder,
    key_extractor::GlobalKeyExtractor,
    GovernorLayer,
};
use tracing::{debug, warn};

use crate::catalog::{ContentKind, TitleVersions};
use crate::serve_files::{sanitize_relative_path, stream_with_range_support};

use super::auth::ensure_authorized;
use super::error::ApiError;
use super::responses::{
    build_catalog_response, build_shop_root_files, build_shop_sections_payload, catalog_sections,
    map_file_error, map_shop_files, map_to_entries, static_png_response,
    CatalogResponse, HealthResponse, SavesListResponse, SearchQuery, SearchResponse,
    SectionsResponse, ShopRootResponse, ShopSectionsQuery, ShopSectionsResponse,
};
use super::state::AppState;

pub fn router(state: AppState) -> Router {
    let governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(20)
            .burst_size(50)
            .key_extractor(GlobalKeyExtractor)
            .finish()
            .expect("default governor config is valid"),
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
        .layer(GovernorLayer::new(governor_conf))
        .layer(
            tower_http::request_id::SetRequestIdLayer::new(
                axum::http::header::HeaderName::from_static("x-request-id"),
                tower_http::request_id::MakeRequestUuid::default(),
            ),
        )
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
    let entries = map_to_entries(catalog.files());
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
    Query(query): Query<ShopSectionsQuery>,
    headers: HeaderMap,
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
    headers: HeaderMap,
    Query(params): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, ApiError> {
    ensure_authorized(&state, &headers)?;

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

    let response = match stream_with_range_support(&state.library_root, &sanitized, &headers).await {
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

    let response =
        match stream_with_range_support(&state.library_root, &relative_path, &headers).await {
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
    Path(_title_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers)?;
    Ok(static_png_response())
}

async fn shop_banner(
    State(state): State<AppState>,
    Path(_title_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_authorized(&state, &headers)?;
    Ok(static_png_response())
}

async fn saves_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SavesListResponse>, ApiError> {
    ensure_authorized(&state, &headers)?;
    Ok(Json(SavesListResponse {
        success: true,
        saves: Vec::new(),
    }))
}
