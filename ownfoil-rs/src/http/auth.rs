use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use base64::prelude::*;
use tracing::{debug, warn};

use super::error::ApiError;
use super::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Shop,
    Backup,
    Admin,
}

pub async fn ensure_authorized(
    state: &AppState,
    headers: &HeaderMap,
    session_token: Option<&str>,
) -> Result<(), ApiError> {
    ensure_access(state, headers, session_token, Access::Shop).await
}

pub async fn ensure_access(
    state: &AppState,
    headers: &HeaderMap,
    session_token: Option<&str>,
    access: Access,
) -> Result<(), ApiError> {
    if access == Access::Shop && state.settings.read().await.shop.public {
        return Ok(());
    }
    if !state.auth.is_enabled() {
        return Ok(());
    }

    let username = session_token
        .and_then(|token| state.sessions.get(token))
        .or_else(|| {
            extract_basic_auth(headers).and_then(|(username, password)| {
                state.auth.is_authorized(&username, &password).then_some(username)
            })
        })
        .ok_or_else(|| {
            warn!("unauthorized request");
            ApiError::Unauthorized
        })?;
    let roles = state.auth.roles(&username).ok_or(ApiError::Unauthorized)?;
    let allowed = match access {
        Access::Shop => roles.shop_access || roles.admin_access,
        Access::Backup => roles.backup_access || roles.admin_access,
        Access::Admin => roles.admin_access,
    };
    if allowed {
        debug!(%username, ?access, "authorized request");
        return Ok(());
    }

    warn!(%username, ?access, "forbidden request");
    Err(ApiError::Forbidden)
}

pub fn extract_basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let raw = headers.get(AUTHORIZATION).and_then(|value| value.to_str().ok())?;
    let mut parts = raw.split_whitespace();
    let scheme = parts.next()?;
    let encoded = parts.next()?;
    if !scheme.eq_ignore_ascii_case("basic") || parts.next().is_some() {
        return None;
    }
    let decoded = BASE64_STANDARD.decode(encoded).ok()?;
    let credentials = String::from_utf8(decoded).ok()?;
    let (username, password) = credentials.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}
