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
mod content;
mod http;
mod identifier;
mod keys;
mod organizer;
mod scanner;
mod serve_files;
mod settings;
mod shop;
mod storage;
mod tasks;
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
    let catalog = Arc::new(RwLock::new(Catalog::from_files(Vec::new())));
    let settings = Arc::new(RwLock::new(config.settings));
    let scan_lock = Arc::new(tokio::sync::Mutex::new(()));

    spawn_file_watcher(Arc::clone(&settings), storage.clone());

    let (titledb_progress_tx, _) = tokio::sync::broadcast::channel::<String>(16);
    let titledb = TitleDb::with_progress(
        config.titledb.clone(),
        config.data_dir.clone(),
        Some(titledb_progress_tx.clone()),
    );
    let refresh_interval = config.titledb.refresh_interval.as_str();
    spawn_titledb_refresh(titledb.clone(), Arc::clone(&settings), storage.clone());
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

    if let Some(storage) = &state.storage {
        for (id, record) in storage.title_override_records().await? {
            state.titledb.set_override(&id, Some(&record)).await?;
        }
    }
    crate::content::recover(&state).await?;
    let current = state.settings.read().await.clone();
    let files = scan_all_libraries(
        &current.library.paths,
        state.storage.as_ref().context("storage unavailable")?,
        &current.library.management,
        &state.keys_path,
    )
    .await?;
    info!(files = files.len(), "library scan complete");
    *state.catalog.write().await = Catalog::from_files(files);
    crate::tasks::start(state.clone()).await?;
    crate::tasks::queue_pipeline(&state).await?;
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
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        // nx-archive trace messages contain decrypted key material.
        .add_directive(
            "nx_archive=off"
                .parse()
                .unwrap_or_else(|_| tracing::level_filters::LevelFilter::OFF.into()),
        );

    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).compact().init();
}

/// Spawns a background task that refreshes `TitleDB` at the given interval (e.g. `24h`).
/// Runs one refresh immediately, then on a ticker.
fn spawn_titledb_refresh(
    titledb: TitleDb,
    settings: Arc<RwLock<crate::settings::Settings>>,
    storage: Storage,
) {
    tokio::spawn(async move {
        let mut previous = String::new();
        let mut startup = true;
        loop {
            let interval = settings.read().await.scheduler.scan_interval.clone();
            if titledb.config().await.enabled {
                if startup && titledb.entry_count().await == 0 {
                    if let Err(error) =
                        crate::tasks::enqueue(&storage, "update_titledb", serde_json::json!({}))
                            .await
                    {
                        error!(%error,"failed to queue initial TitleDB refresh");
                    }
                }
                if let Err(error) =
                    crate::tasks::schedule_titledb(&storage, &interval, previous != interval).await
                {
                    error!(%error,"failed to schedule TitleDB refresh");
                }
            }
            previous = interval;
            startup = false;
            tokio::time::sleep(Duration::from_secs(1)).await;
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

fn spawn_file_watcher(settings: Arc<RwLock<crate::settings::Settings>>, storage: Storage) {
    tokio::spawn(async move {
        loop {
            let library = settings.read().await.library.clone();
            if !library.watcher.enabled {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            let roots = library.paths.clone();
            let mut fingerprint = library_fingerprint(&roots);
            let mut poll_at =
                tokio::time::Instant::now() + Duration::from_secs(library.watcher.polling_interval);
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
                        if matches!(event.kind, notify::EventKind::Access(_)) { continue; }
                        if !matches!(event.kind, notify::EventKind::Remove(_)) && !wait_for_stable_files(&event.paths).await { continue; }
                        while event_rx.try_recv().is_ok() {}
                        if let Err(error)=crate::tasks::enqueue(&storage,"scan_libraries",serde_json::json!({})).await {error!(%error,"failed to queue watcher reconciliation");}

                    }
                    () = tokio::time::sleep(Duration::from_secs(1)) => {
                        let current = settings.read().await.library.clone();
                        if current.paths != roots || current.watcher != library.watcher { break; }
                        if tokio::time::Instant::now() >= poll_at {
                            poll_at = tokio::time::Instant::now() + Duration::from_secs(current.watcher.polling_interval);
                            let observed = library_fingerprint(&roots);
                            if observed != fingerprint {
                                let paths = roots.clone();
                                if !wait_for_stable_files(&paths).await { continue; }
                                if let Err(error)=crate::tasks::enqueue(&storage,"scan_libraries",serde_json::json!({})).await {error!(%error,"failed to queue polling reconciliation");}
                                fingerprint=observed;
                            }
                        }
                    }
                }
            }
        }
    });
}

async fn wait_for_stable_files(paths: &[std::path::PathBuf]) -> bool {
    // Directory events must inspect their contents, not the directory inode size.
    let mut previous = library_fingerprint(paths);
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let current = library_fingerprint(paths);
        if current == previous {
            return true;
        }
        previous = current;
    }
    // A long-running copy is picked up by a later event or polling pass.
    false
}

// Polling also reconciles remote changes that native filesystem notifications miss.
fn library_fingerprint(roots: &[std::path::PathBuf]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut entries = Vec::new();
    for root in roots {
        for entry in walkdir::WalkDir::new(root).into_iter().filter_map(Result::ok) {
            if crate::scanner::is_supported_content(entry.path()) {
                if let Ok(metadata) = entry.metadata() {
                    entries.push((
                        entry.path().to_path_buf(),
                        metadata.len(),
                        metadata.modified().ok(),
                    ));
                }
            }
        }
    }
    entries.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for entry in &entries {
        entry.hash(&mut hasher);
    }
    hasher.finish()
}
