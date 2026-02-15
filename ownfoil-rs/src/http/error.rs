use axum::http::header::WWW_AUTHENTICATE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
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

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::TitleNotFound | ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::InvalidPath => StatusCode::BAD_REQUEST,
            ApiError::InvalidRange => StatusCode::RANGE_NOT_SATISFIABLE,
            ApiError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn is_unauthorized(&self) -> bool {
        matches!(self, ApiError::Unauthorized)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({ "error": self.to_string() }));
        let mut response = (self.status(), body).into_response();
        if self.is_unauthorized() {
            response.headers_mut().insert(
                WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"ownfoil-rs\""),
            );
        }
        response
    }
}
