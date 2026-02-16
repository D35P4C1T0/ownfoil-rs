use axum::http::header::AUTHORIZATION;
use axum::http::HeaderMap;
use base64::prelude::*;
use tracing::{debug, warn};

use super::error::ApiError;
use super::state::AppState;

pub fn ensure_authorized(
    state: &AppState,
    headers: &HeaderMap,
    session_token: Option<&str>,
) -> Result<(), ApiError> {
    if !state.auth.is_enabled() {
        return Ok(());
    }

    if let Some(token) = session_token {
        if state.sessions.get(token).is_some() {
            debug!("authorized request using session");
            return Ok(());
        }
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

pub fn extract_basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let raw = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())?;
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
