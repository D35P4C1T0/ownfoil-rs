#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod auth;
mod catalog;
mod config;
mod http;
mod scanner;
mod serve_files;

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::Context;
use axum::serve;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::auth::{load_users_from_file, AuthSettings};
use crate::catalog::Catalog;
use crate::config::{AppConfig, Cli};
use crate::http::{router, AppState};
use crate::scanner::scan_library;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();

    let cli = Cli::parse();
    let config = AppConfig::from_cli(cli).context("failed to load configuration")?;
    let users = if config.public_shop {
        if config.auth_file.is_some() {
            info!("public shop mode enabled; auth file is ignored");
        }
        Vec::new()
    } else {
        let auth_path = config.auth_file.as_deref().ok_or_else(|| {
            anyhow!("private shop requires auth credentials file. Set --auth-file or OWNFOIL_PUBLIC=true")
        })?;
        load_users_from_file(Some(auth_path)).context("failed to load auth credentials file")?
    };
    let auth = AuthSettings::from_users(users);
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

    let state = AppState {
        catalog,
        library_root: config.library_root,
        auth: Arc::new(auth),
    };

    let app = router(state);
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;

    info!(bind = %config.bind, "ownfoil-rs listening");

    serve(listener, app)
        .await
        .context("server exited with error")
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

fn spawn_background_scanner(
    catalog: Arc<RwLock<Catalog>>,
    root: std::path::PathBuf,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);

        loop {
            ticker.tick().await;

            match scan_library(&root).await {
                Ok(files) => {
                    let count = files.len();
                    let mut write_guard = catalog.write().await;
                    *write_guard = Catalog::from_files(files);
                    info!(files = count, "catalog refreshed");
                }
                Err(err) => {
                    error!(error = %err, "catalog refresh failed");
                }
            }
        }
    });
}
