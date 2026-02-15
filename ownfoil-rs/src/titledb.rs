//! TitleDB integration: fetch game metadata (icon/banner URLs) from multiple sources.
//! Fetches concurrently from all sources and merges results redundantly.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};

use crate::config::TitleDbConfig;

/// Per-title metadata from TitleDB.
#[derive(Debug, Clone)]
pub struct TitleInfo {
    pub icon_url: Option<String>,
    pub banner_url: Option<String>,
    pub name: Option<String>,
}

/// Lazy-loaded TitleDB cache. Loads from disk on first access, refreshes in background.
#[derive(Debug, Clone)]
pub struct TitleDb {
    inner: Arc<RwLock<TitleDbInner>>,
}

#[derive(Debug)]
struct TitleDbInner {
    map: HashMap<String, TitleInfo>,
    config: TitleDbConfig,
    data_dir: PathBuf,
    last_refresh: Option<std::time::Instant>,
    progress_tx: Option<broadcast::Sender<String>>,
}

impl TitleDb {
    pub fn new(config: TitleDbConfig, data_dir: PathBuf) -> Self {
        Self::with_progress(config, data_dir, None)
    }

    pub fn with_progress(
        config: TitleDbConfig,
        data_dir: PathBuf,
        progress_tx: Option<broadcast::Sender<String>>,
    ) -> Self {
        info!(
            enabled = config.enabled,
            region = %config.region,
            language = %config.language,
            refresh_interval = %config.refresh_interval,
            data_dir = %data_dir.display(),
            "titledb initialized"
        );
        Self {
            inner: Arc::new(RwLock::new(TitleDbInner {
                map: HashMap::new(),
                config,
                data_dir,
                last_refresh: None,
                progress_tx,
            })),
        }
    }

    pub async fn progress_subscribe(&self) -> Option<broadcast::Receiver<String>> {
        self.inner
            .read()
            .await
            .progress_tx
            .as_ref()
            .map(|tx| tx.subscribe())
    }

    /// Look up icon and banner URLs for a title ID (16-char hex, uppercase).
    pub async fn lookup(&self, title_id: &str) -> Option<TitleInfo> {
        let normalized = title_id.to_uppercase();
        let guard = self.inner.read().await;
        guard.map.get(&normalized).cloned()
    }

    /// Trigger a refresh. Returns immediately; refresh runs in background.
    /// Fetch runs without holding the lock so lookups remain fast during refresh.
    pub fn refresh(&self) {
        debug!("titledb refresh triggered");
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            if let Err(e) = do_refresh_without_lock(&inner).await {
                error!(error = %e, "titledb refresh failed");
            }
        });
    }

    pub async fn config(&self) -> TitleDbConfig {
        self.inner.read().await.config.clone()
    }

    pub async fn set_config(&self, config: TitleDbConfig) {
        self.inner.write().await.config = config;
    }

    pub async fn last_refresh(&self) -> Option<std::time::Instant> {
        self.inner.read().await.last_refresh
    }

    pub async fn entry_count(&self) -> usize {
        self.inner.read().await.map.len()
    }
}

fn send_progress(tx: &Option<broadcast::Sender<String>>, msg: &str) {
    if let Some(tx) = tx {
        let _ = tx.send(msg.to_string());
    }
}

/// Fetch and merge TitleDB data without holding the lock, then apply in a short write.
async fn do_refresh_without_lock(inner: &RwLock<TitleDbInner>) -> Result<(), TitleDbError> {
    let (enabled, region, lang, url_override, data_dir, progress_tx) = {
        let guard = inner.read().await;
        if !guard.config.enabled {
            debug!("titledb refresh skipped (disabled)");
            return Ok(());
        }
        (
            guard.config.enabled,
            guard.config.region.clone(),
            guard.config.language.clone(),
            guard.config.url_override.clone(),
            guard.data_dir.clone(),
            guard.progress_tx.clone(),
        )
    };
    if !enabled {
        return Ok(());
    }

    send_progress(&progress_tx, "[titledb] refresh starting");
    info!(
        region = %region,
        language = %lang,
        "titledb refresh starting"
    );

    let cache_path = data_dir
        .join("titledb")
        .join(format!("{region}.{lang}.json"));

    std::fs::create_dir_all(cache_path.parent().unwrap())?;
    debug!(cache_path = %cache_path.display(), "titledb cache path");

    send_progress(&progress_tx, "[titledb] fetching from multiple sources...");

    // jsDelivr has a 20 MB limit for GitHub files; TitleDB JSON exceeds that
    let blawar_raw = Source::BlawarRaw {
        url: format!(
            "https://raw.githubusercontent.com/blawar/titledb/master/{region}.{lang}.json"
        ),
    };

    let mut sources: Vec<Source> = vec![blawar_raw];
    if let Some(url) = url_override {
        sources.push(Source::OwnfoilZip { url });
    }

    let merged = fetch_and_merge(&sources, &region, &lang, &progress_tx).await?;

    send_progress(&progress_tx, "[titledb] applying updates...");

    let mut guard = inner.write().await;
    if !merged.is_empty() {
        let count = merged.len();
        guard.map = merged;
        guard.last_refresh = Some(std::time::Instant::now());
        send_progress(
            &progress_tx,
            &format!("[titledb] loaded {} entries from network", count),
        );
        info!(entries = count, "titledb loaded from network");

        if let Err(e) = save_cache(&cache_path, &guard.map) {
            warn!(path = %cache_path.display(), error = %e, "titledb cache save failed");
        } else {
            send_progress(&progress_tx, "[titledb] cache saved");
            debug!(path = %cache_path.display(), "titledb cache saved");
        }
    } else {
        send_progress(&progress_tx, "[titledb] network empty, trying cache...");
        info!("titledb network fetch returned no data, trying cache");
        if cache_path.exists() {
            match load_cache(&cache_path) {
                Ok(loaded) => {
                    let count = loaded.len();
                    guard.map = loaded;
                    guard.last_refresh = Some(std::time::Instant::now());
                    send_progress(
                        &progress_tx,
                        &format!("[titledb] loaded {} entries from cache", count),
                    );
                    info!(
                        entries = count,
                        path = %cache_path.display(),
                        "titledb loaded from cache"
                    );
                }
                Err(e) => {
                    warn!(
                        path = %cache_path.display(),
                        error = %e,
                        "titledb cache load failed"
                    );
                }
            }
        } else {
            send_progress(&progress_tx, "[titledb] empty, no cache available");
            warn!(
                path = %cache_path.display(),
                "titledb empty and no cache available"
            );
        }
    }

    send_progress(&progress_tx, "[titledb] refresh complete");
    Ok(())
}

async fn fetch_and_merge(
    sources: &[Source],
    region: &str,
    lang: &str,
    progress_tx: &Option<broadcast::Sender<String>>,
) -> Result<HashMap<String, TitleInfo>, TitleDbError> {
    let mut merged = HashMap::new();

    let handles: Vec<_> = sources
        .iter()
        .map(|src| fetch_source(src, region, lang))
        .collect();

    let results = futures_util::future::join_all(handles).await;

    let source_names: Vec<&str> = sources
        .iter()
        .map(|s| match s {
            Source::OwnfoilZip { .. } => "ownfoil_zip",
            Source::BlawarRaw { url } => {
                if url.contains("jsdelivr") {
                    "blawar_jsdelivr"
                } else {
                    "blawar_raw"
                }
            }
        })
        .collect();

    for (name, result) in source_names.iter().zip(results.iter()) {
        match result {
            Ok(entries) => {
                let count = entries.len();
                send_progress(
                    progress_tx,
                    &format!("[titledb] {} fetched {} entries", name, count),
                );
                for (id, info) in entries {
                    merged
                        .entry(id.clone())
                        .or_insert_with(|| TitleInfo {
                            icon_url: None,
                            banner_url: None,
                            name: None,
                        })
                        .merge(&info);
                }
                info!(source = %name, entries = count, "titledb source fetched");
            }
            Err(e) => {
                send_progress(progress_tx, &format!("[titledb] {} failed: {}", name, e));
                warn!(source = %name, error = %e, "titledb source fetch failed");
            }
        }
    }

    Ok(merged)
}

#[derive(Debug, Clone)]
enum Source {
    OwnfoilZip { url: String },
    BlawarRaw { url: String },
}

async fn fetch_source(
    source: &Source,
    region: &str,
    lang: &str,
) -> Result<Vec<(String, TitleInfo)>, TitleDbError> {
    match source {
        Source::OwnfoilZip { url } => fetch_ownfoil_zip(url, region, lang).await,
        Source::BlawarRaw { url } => fetch_blawar_raw(url).await,
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .user_agent("ownfoil-rs/1.0 (TitleDB metadata fetcher)")
        .build()
        .unwrap_or_default()
}

async fn fetch_ownfoil_zip(
    zip_url: &str,
    region: &str,
    lang: &str,
) -> Result<Vec<(String, TitleInfo)>, TitleDbError> {
    let client = http_client();
    let resp = client.get(zip_url).send().await.map_err(|e| {
        warn!(url = %zip_url, error = %e, "titledb ownfoil zip: connection failed");
        TitleDbError::Http(e)
    })?;
    let status = resp.status();
    if !status.is_success() {
        warn!(
            url = %zip_url,
            status = %status,
            "titledb ownfoil zip: HTTP error"
        );
        return Err(TitleDbError::Http(resp.error_for_status().unwrap_err()));
    }
    let bytes = resp.bytes().await?;
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)?;

    let titles_name = format!("titles.{region}.{lang}.json");
    let alt_name = format!("{region}.{lang}.json");

    let has_titles = archive.by_name(&titles_name).is_ok();
    let mut file = if has_titles {
        archive.by_name(&titles_name).unwrap()
    } else {
        archive.by_name(&alt_name)?
    };

    let mut buf = String::new();
    std::io::Read::read_to_string(&mut file, &mut buf)?;
    parse_titles_json(&buf)
}

async fn fetch_blawar_raw(url: &str) -> Result<Vec<(String, TitleInfo)>, TitleDbError> {
    let client = http_client();
    let resp = client.get(url).send().await.map_err(|e| {
        warn!(url = %url, error = %e, "titledb blawar: connection failed (DNS/network?)");
        TitleDbError::Http(e)
    })?;
    let status = resp.status();
    if !status.is_success() {
        warn!(
            url = %url,
            status = %status,
            "titledb blawar: HTTP error"
        );
        return Err(TitleDbError::Http(resp.error_for_status().unwrap_err()));
    }
    let bytes = resp.bytes().await?;
    let buf = String::from_utf8(bytes.to_vec())?;
    parse_titles_json(&buf)
}

fn parse_titles_json(buf: &str) -> Result<Vec<(String, TitleInfo)>, TitleDbError> {
    let raw: serde_json::Value = serde_json::from_str(buf)?;
    let obj = raw.as_object().ok_or(TitleDbError::InvalidFormat)?;

    let mut out = Vec::new();
    for (_, v) in obj {
        let entry = v.as_object().ok_or(TitleDbError::InvalidFormat)?;
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_uppercase());
        let Some(id) = id else { continue };
        if id.len() != 16 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }

        let mut icon_url = entry
            .get("iconUrl")
            .or_else(|| entry.get("icon_url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(ref url) = icon_url {
            if !url.is_empty() && !url.starts_with("http") {
                icon_url = Some(format!("https://img-eshop.cdn.nintendo.net{}", url));
            }
        }
        let banner_url = entry
            .get("bannerUrl")
            .or_else(|| entry.get("banner_url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        out.push((
            id,
            TitleInfo {
                icon_url,
                banner_url,
                name,
            },
        ));
    }

    Ok(out)
}

impl TitleInfo {
    fn merge(&mut self, other: &Self) {
        if self.icon_url.is_none() && other.icon_url.is_some() {
            self.icon_url = other.icon_url.clone();
        }
        if self.banner_url.is_none() && other.banner_url.is_some() {
            self.banner_url = other.banner_url.clone();
        }
        if self.name.is_none() && other.name.is_some() {
            self.name = other.name.clone();
        }
    }
}

fn load_cache(path: &std::path::Path) -> Result<HashMap<String, TitleInfo>, TitleDbError> {
    let buf = std::fs::read_to_string(path)?;
    let raw: Vec<serde_json::Value> = serde_json::from_str(&buf)?;
    let mut map = HashMap::new();
    for v in raw {
        let obj = v.as_object().ok_or(TitleDbError::InvalidFormat)?;
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_uppercase());
        let Some(id) = id else { continue };
        let icon_url = obj
            .get("icon_url")
            .and_then(|v| v.as_str())
            .map(String::from);
        let banner_url = obj
            .get("banner_url")
            .and_then(|v| v.as_str())
            .map(String::from);
        let name = obj.get("name").and_then(|v| v.as_str()).map(String::from);
        map.insert(
            id,
            TitleInfo {
                icon_url,
                banner_url,
                name,
            },
        );
    }
    Ok(map)
}

fn save_cache(
    path: &std::path::Path,
    map: &HashMap<String, TitleInfo>,
) -> Result<(), TitleDbError> {
    let arr: Vec<serde_json::Value> = map
        .iter()
        .map(|(id, info)| {
            serde_json::json!({
                "id": id,
                "icon_url": info.icon_url,
                "banner_url": info.banner_url,
                "name": info.name,
            })
        })
        .collect();
    let buf = serde_json::to_string_pretty(&arr)?;
    std::fs::write(path, buf)?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum TitleDbError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("ZIP error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("invalid format")]
    InvalidFormat,
}
