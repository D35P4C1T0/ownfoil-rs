//! # ownfoil-rs
//!
//! Barebones CyberFoil-compatible Tinfoil game server in Rust.
//!
//! Serves a Nintendo Switch content library over HTTP with catalog listing, file download,
//! and optional HTTP Basic auth. Compatible with Tinfoil and CyberFoil clients.
//!
//! ## Architecture
//!
//! - **Catalog**: In-memory index of `.nsp`, `.xci`, `.nsz`, `.xcz` files, refreshed on interval
//! - **TitleDB**: Optional game metadata (icons, banners) from [blawar/titledb](https://github.com/blawar/titledb)
//! - **Auth**: TOML-based credentials with constant-time password comparison
//! - **HTTP**: Axum router with rate limiting, request IDs, and graceful shutdown

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod auth;
mod catalog;
mod config;
mod http;
mod scanner;
mod serve_files;
mod titledb;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::serve;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::auth::load_auth;
use crate::catalog::Catalog;
use crate::config::{AppConfig, Cli};
use crate::http::{router, AppState, SessionStore};
use crate::scanner::scan_library;
use crate::titledb::TitleDb;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging().context("failed to initialize logging")?;

    let cli = Cli::parse();
    let config = AppConfig::from_cli(cli).context("failed to load configuration")?;
    let auth = if config.public_shop {
        if config.auth_file.is_some() {
            info!("public shop mode enabled; auth file is ignored");
        }
        load_auth(None).context("failed to initialize auth")?
    } else {
        let auth_path = config
            .auth_file
            .as_deref()
            .unwrap_or_else(|| unreachable!("validated by config"));
        load_auth(Some(auth_path)).context("failed to load auth credentials file")?
    };
    info!(
        bind = %config.bind,
        root = %config.library_root.display(),
        public_shop = config.public_shop,
        auth_enabled = auth.is_enabled(),
        auth_user_count = auth.user_count(),
        auth_file = ?config.auth_file.as_ref().map(|path| path.display().to_string()),
        scan_interval_seconds = config.scan_interval_seconds,
        "configuration loaded"
    );

    let initial_files = scan_library(&config.library_root).await.with_context(|| {
        format!(
            "failed to scan library root {}",
            config.library_root.display()
        )
    })?;

    info!(
        files = initial_files.len(),
        root = %config.library_root.display(),
        "library scan complete"
    );

    let catalog = Arc::new(RwLock::new(Catalog::from_files(initial_files)));

    spawn_background_scanner(
        Arc::clone(&catalog),
        config.library_root.clone(),
        Duration::from_secs(config.scan_interval_seconds),
    );

    let (titledb_progress_tx, _) = tokio::sync::broadcast::channel::<String>(16);
    let titledb = TitleDb::with_progress(
        config.titledb.clone(),
        config.data_dir.clone(),
        Some(titledb_progress_tx.clone()),
    );
    let refresh_interval = config.titledb.refresh_interval.as_str();
    spawn_titledb_refresh(titledb.clone(), refresh_interval);
    if config.titledb.enabled {
        info!(
            refresh_interval = %refresh_interval,
            "titledb background refresh scheduled"
        );
    }

    let state = AppState {
        catalog,
        library_root: config.library_root,
        auth: Arc::new(auth),
        sessions: SessionStore::new(24),
        titledb,
        data_dir: config.data_dir,
        titledb_progress_tx,
    };

    let app = router(state);
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;

    if config.bind.ip().is_loopback() {
        tracing::warn!(
            bind = %config.bind,
            "binding to loopback; use --bind 0.0.0.0:8465 for LAN access"
        );
    }

    let shutdown = tokio::signal::ctrl_c();
    info!(bind = %config.bind, "ownfoil-rs listening");

    serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = shutdown.await;
        info!("shutting down gracefully");
    })
    .await
    .context("server exited with error")
}

/// Initialize tracing subscriber with `RUST_LOG` env filter (default: `info`).
fn init_logging() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    Ok(())
}

/// Spawns a background task that refreshes TitleDB at the given interval (e.g. `24h`).
/// Runs one refresh immediately, then on a ticker.
fn spawn_titledb_refresh(titledb: TitleDb, interval_str: &str) {
    let interval = humantime::parse_duration(interval_str).unwrap_or(Duration::from_secs(86400));
    tokio::spawn(async move {
        titledb.refresh();
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            titledb.refresh();
        }
    });
}

/// Spawns a background task that rescans the library root at the given interval.
/// Updates the shared catalog in place. Logs errors but does not panic.
fn spawn_background_scanner(
    catalog: Arc<RwLock<Catalog>>,
    root: std::path::PathBuf,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);

        loop {
            ticker.tick().await;

            let root = root.clone();
            let catalog = Arc::clone(&catalog);
            let handle = tokio::spawn(async move {
                let files = scan_library(&root).await?;
                let count = files.len();
                let mut guard = catalog.write().await;
                *guard = Catalog::from_files(files);
                Ok::<_, crate::scanner::ScanError>(count)
            });

            match handle.await {
                Ok(Ok(count)) => info!(files = count, "catalog refreshed"),
                Ok(Err(err)) => error!(error = %err, "catalog refresh failed"),
                Err(join_err) => {
                    if join_err.is_panic() {
                        error!(
                            error = %join_err,
                            "catalog scanner panicked; will retry on next interval"
                        );
                    }
                }
            }
        }
    });
}
