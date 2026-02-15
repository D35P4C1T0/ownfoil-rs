//! HTTP layer: Axum router, handlers, and responses.
//!
//! Exposes CyberFoil-compatible endpoints (`/`, `/api/shop/sections`, etc.),
//! admin UI, and settings API.

mod auth;
mod error;
mod handlers;
mod responses;
mod settings;
mod state;

#[cfg(test)]
mod tests;

pub use handlers::router;
pub use state::{AppState, SessionStore};
