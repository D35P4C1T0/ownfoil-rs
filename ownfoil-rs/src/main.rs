//! # ownfoil-rs
//!
//! Barebones CyberFoil-compatible Tinfoil game server in Rust.
//!
//! Serves a Nintendo Switch content library over HTTP with catalog listing, file download,
//! and optional HTTP Basic auth. Compatible with Tinfoil and `CyberFoil` clients.
//!
//! ## Architecture
//!
//! - **Catalog**: In-memory index of `.nsp`, `.xci`, `.nsz`, `.xcz` files, refreshed on interval
//! - **`TitleDB`**: Optional game metadata (icons, banners) from [blawar/titledb](https://github.com/blawar/titledb)
//! - **Auth**: TOML-based credentials with constant-time password comparison
//! - **HTTP**: Axum router with rate limiting, request IDs, and graceful shutdown

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod auth;
mod catalog;
mod config;
mod http;
mod identifier;
mod keys;
mod organizer;
mod scanner;
mod serve_files;
mod settings;
mod shop;
mod storage;
mod titledb;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::serve;
use clap::Parser;
use notify::Watcher as _;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::auth::{AuthRoles, hash_password, load_auth};
use crate::catalog::Catalog;
use crate::config::{AppConfig, Cli};
use crate::http::{AppState, SessionStore, router};
use crate::scanner::scan_library;
use crate::storage::{NewUser, Storage};
use crate::titledb::TitleDb;

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    init_logging();

    let cli = Cli::parse();
    let config = AppConfig::from_cli(cli).context("failed to load configuration")?;
    std::fs::create_dir_all(&config.config_dir)
        .with_context(|| format!("failed to create {}", config.config_dir.display()))?;
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("failed to create {}", config.data_dir.display()))?;
    config.settings.save(&config.settings_path).context("failed to persist Ownfoil settings")?;
    let auth = load_auth(config.auth_file.as_deref()).context("failed to load auth credentials")?;
    info!(
        bind = %config.bind,
        root = %config.library_root.display(),
        public_shop = config.public_shop,
        insecure_admin_cookie = config.insecure_admin_cookie,
        shop_encrypt = config.shop.effective_encrypt(),
        shop_tinfoil_only_mode = config.shop.tinfoil_only_mode,
        auth_enabled = auth.is_enabled(),
        auth_user_count = auth.user_count(),
        auth_file = ?config.auth_file.as_ref().map(|path| path.display().to_string()),
        scan_interval_seconds = config.scan_interval_seconds,
        "configuration loaded"
    );

    let storage = Storage::open(&config.db_path).await.context("failed to initialize storage")?;
    hydrate_auth_from_storage_and_environment(&storage, &auth).await?;
    let initial_files = scan_all_libraries(
        &config.library_roots,
        &storage,
        &config.settings.library.management,
        &config.keys_path,
    )
    .await?;

    info!(files = initial_files.len(), roots = config.library_roots.len(), "library scan complete");

    let catalog = Arc::new(RwLock::new(Catalog::from_files(initial_files)));
    let settings = Arc::new(RwLock::new(config.settings));
    let scan_lock = Arc::new(tokio::sync::Mutex::new(()));

    spawn_background_scanner(
        Arc::clone(&catalog),
        Arc::clone(&settings),
        storage.clone(),
        Arc::clone(&scan_lock),
        config.keys_path.clone(),
        Duration::from_secs(config.scan_interval_seconds),
    );
    spawn_file_watcher(
        Arc::clone(&catalog),
        Arc::clone(&settings),
        storage.clone(),
        Arc::clone(&scan_lock),
        config.keys_path.clone(),
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
        storage: Some(storage),
        scan_lock,
        settings,
        settings_path: config.settings_path,
        keys_path: config.keys_path,
        auth: Arc::new(auth),
        shop: Arc::new(RwLock::new(config.shop)),
        insecure_admin_cookie: config.insecure_admin_cookie,
        sessions: SessionStore::new(24),
        titledb,
        titles_cache: Arc::new(RwLock::new(None)),
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
    if config.insecure_admin_cookie {
        tracing::warn!(
            "OWNFOIL_INSECURE_ADMIN_COOKIE=true; admin session cookie will be sent over HTTP"
        );
    }

    info!(bind = %config.bind, "ownfoil-rs listening");

    serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            shutdown_signal().await;
            info!("shutting down gracefully");
        })
        .await
        .context("server exited with error")
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        use std::future::pending;
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = ctrl_c => {}
            () = async {
                if let Some(terminate) = terminate.as_mut() {
                    terminate.recv().await;
                } else {
                    pending::<()>().await;
                }
            } => {
            }
        }
    }

    #[cfg(not(unix))]
    if let Err(error) = ctrl_c.await {
        error!(%error, "failed to listen for shutdown signal");
    }
}

async fn hydrate_auth_from_storage_and_environment(
    storage: &Storage,
    auth: &crate::auth::AuthSettings,
) -> anyhow::Result<()> {
    for user in storage.list_users().await.context("failed to load stored users")? {
        if user.enabled {
            auth.upsert_hashed_user(
                user.username,
                user.password_hash,
                AuthRoles {
                    admin_access: user.can_admin,
                    shop_access: user.can_download,
                    backup_access: user.can_upload,
                },
            );
        }
    }

    for (prefix, admin) in [("USER_ADMIN", true), ("USER_GUEST", false)] {
        let (Ok(username), Ok(password)) =
            (std::env::var(format!("{prefix}_NAME")), std::env::var(format!("{prefix}_PASSWORD")))
        else {
            continue;
        };
        if !admin
            && !storage
                .list_users()
                .await
                .context("failed to verify bootstrap administrator")?
                .iter()
                .any(|user| user.enabled && user.can_admin)
        {
            tracing::warn!(username, "ignoring guest bootstrap until an admin exists");
            continue;
        }
        let password_hash =
            hash_password(&password).context("failed to hash bootstrap password")?;
        let roles = AuthRoles { admin_access: admin, shop_access: true, backup_access: admin };
        storage
            .upsert_user(NewUser {
                username: username.clone(),
                password_hash: password_hash.clone(),
                can_admin: roles.admin_access,
                can_upload: roles.backup_access,
                can_download: roles.shop_access,
                enabled: true,
            })
            .await
            .context("failed to persist bootstrap user")?;
        auth.upsert_hashed_user(username, password_hash, roles);
    }
    Ok(())
}

/// Initialize tracing subscriber with `RUST_LOG` env filter (default: `info`).
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).compact().init();
}

/// Spawns a background task that refreshes `TitleDB` at the given interval (e.g. `24h`).
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
    settings: Arc<RwLock<crate::settings::Settings>>,
    storage: Storage,
    scan_lock: Arc<tokio::sync::Mutex<()>>,
    keys_path: std::path::PathBuf,
    fallback_interval: Duration,
) {
    tokio::spawn(async move {
        loop {
            let interval = {
                let settings = settings.read().await;
                if settings.scheduler.scan_interval == "0" {
                    None
                } else {
                    humantime::parse_duration(&settings.scheduler.scan_interval).ok()
                }
            };
            tokio::time::sleep(interval.unwrap_or(fallback_interval).max(Duration::from_secs(1)))
                .await;
            if interval.is_none() {
                continue;
            }

            let (roots, management) = {
                let settings = settings.read().await;
                (settings.library.paths.clone(), settings.library.management.clone())
            };
            let storage = storage.clone();
            let catalog = Arc::clone(&catalog);
            let scan_lock = Arc::clone(&scan_lock);
            let keys_path = keys_path.clone();
            let handle = tokio::spawn(async move {
                let Ok(_guard) = scan_lock.try_lock() else {
                    info!("scheduled library scan skipped because another scan is running");
                    return Ok::<_, crate::scanner::ScanError>(0);
                };
                let files = scan_all_libraries(&roots, &storage, &management, &keys_path)
                    .await
                    .map_err(|error| crate::scanner::ScanError::Walk {
                        path: roots
                            .iter()
                            .map(|root| root.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                        source: std::io::Error::other(error.to_string()),
                    })?;
                let count = files.len();
                *catalog.write().await = Catalog::from_files(files);
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

async fn scan_all_libraries(
    roots: &[std::path::PathBuf],
    storage: &Storage,
    management: &crate::settings::LibraryManagementSettings,
    keys_path: &std::path::Path,
) -> anyhow::Result<Vec<crate::catalog::ContentFile>> {
    let mut all_files = Vec::new();
    for root in roots {
        let mut files = scan_library(root)
            .await
            .with_context(|| format!("failed to scan library root {}", root.display()))?;
        files = crate::identifier::identify_files(root, files, keys_path).await;
        if crate::organizer::organize(root, &files, management)
            .await
            .with_context(|| format!("failed to organize library root {}", root.display()))?
        {
            files = scan_library(root)
                .await
                .with_context(|| format!("failed to rescan library root {}", root.display()))?;
            files = crate::identifier::identify_files(root, files, keys_path).await;
        }
        let files = storage
            .reconcile_library_scan(root.clone(), files)
            .await
            .with_context(|| format!("failed to persist library root {}", root.display()))?;
        all_files.extend(files);
    }
    Ok(all_files)
}

fn spawn_file_watcher(
    catalog: Arc<RwLock<Catalog>>,
    settings: Arc<RwLock<crate::settings::Settings>>,
    storage: Storage,
    scan_lock: Arc<tokio::sync::Mutex<()>>,
    keys_path: std::path::PathBuf,
) {
    tokio::spawn(async move {
        loop {
            let roots = settings.read().await.library.paths.clone();
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
            let watcher = notify::RecommendedWatcher::new(
                move |event| {
                    let _ = event_tx.send(event);
                },
                notify::Config::default().with_poll_interval(Duration::from_secs(2)),
            );
            let Ok(mut watcher) = watcher else {
                error!("failed to initialize library watcher; retrying");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            };
            for root in &roots {
                if let Err(error) =
                    notify::Watcher::watch(&mut watcher, root, notify::RecursiveMode::Recursive)
                {
                    error!(root = %root.display(), error = %error, "failed to watch library");
                }
            }

            loop {
                tokio::select! {
                    event = event_rx.recv() => {
                        let Some(Ok(event)) = event else { break };
                        if !event.paths.iter().any(|path| crate::scanner::is_supported_content(path)) {
                            continue;
                        }
                        if !matches!(event.kind, notify::EventKind::Remove(_)) {
                            wait_for_stable_files(&event.paths).await;
                        }
                        while event_rx.try_recv().is_ok() {}
                        let management = settings.read().await.library.management.clone();
                        let Ok(_guard) = scan_lock.try_lock() else {
                            info!("watcher scan skipped because another scan is running");
                            continue;
                        };
                        match scan_all_libraries(&roots, &storage, &management, &keys_path).await {
                            Ok(files) => {
                                let count = files.len();
                                *catalog.write().await = Catalog::from_files(files);
                                info!(files = count, "catalog refreshed after filesystem event");
                            }
                            Err(error) => error!(error = %error, "watcher refresh failed"),
                        }
                    }
                    () = tokio::time::sleep(Duration::from_secs(10)) => {
                        if settings.read().await.library.paths != roots {
                            break;
                        }
                    }
                }
            }
        }
    });
}

async fn wait_for_stable_files(paths: &[std::path::PathBuf]) {
    let sizes = || {
        paths
            .iter()
            .filter_map(|path| std::fs::metadata(path).ok().map(|metadata| (path, metadata.len())))
            .collect::<Vec<_>>()
    };
    let mut previous = sizes();
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let current = sizes();
        if current == previous {
            return;
        }
        previous = current;
    }
}
