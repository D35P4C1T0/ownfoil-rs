//! `TitleDB` integration: fetch game metadata (icon/banner URLs) from multiple sources.
//! Fetches concurrently from all sources and merges results redundantly.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::{Mutex, broadcast};
use tracing::{debug, error, info, warn};

use crate::config::TitleDbConfig;

/// Per-title metadata from `TitleDB`.
///
/// Used to enrich shop section items with icon/banner URLs and display names.
#[derive(Debug, Clone, Default)]
pub struct TitleInfo {
    /// CDN URL for the game icon (e.g. Nintendo eShop).
    pub icon_url: Option<String>,
    /// CDN URL for the banner image.
    pub banner_url: Option<String>,
    /// Localized game name.
    pub name: Option<String>,
    pub record: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleVersionInfo {
    pub title_id: String,
    pub latest_version: Option<u64>,
    pub versions: Vec<u64>,
    pub release_dates: std::collections::BTreeMap<u64, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleCnmtInfo {
    pub title_id: String,
    pub title_type: Option<String>,
    pub base_title_id: Option<String>,
    pub version: Option<u64>,
    pub required_system_version: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleLanguageInfo {
    pub title_id: String,
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct TitleDbArtifacts {
    versions: HashMap<String, TitleVersionInfo>,
    cnmts: HashMap<String, Vec<TitleCnmtInfo>>,
    languages: HashMap<String, TitleLanguageInfo>,
    regions: HashMap<String, Vec<String>>,
}

/// Release refresh ownership even when a refresh future is cancelled or panics.
struct RefreshGuard(Arc<AtomicBool>);
impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Lazy-loaded `TitleDB` cache. Loads from disk on first access, refreshes in background.
#[derive(Debug, Clone)]
pub struct TitleDb {
    inner: Arc<Mutex<TitleDbInner>>,
    refreshing: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
}

#[derive(Debug)]
struct TitleDbInner {
    db: Connection,
    config: TitleDbConfig,
    data_dir: PathBuf,
    last_refresh: Option<std::time::Instant>,
    progress_tx: Option<broadcast::Sender<String>>,
}

#[allow(dead_code)]
impl TitleDb {
    #[allow(dead_code)]
    pub fn new(config: TitleDbConfig, data_dir: PathBuf) -> Self {
        Self::with_progress(config, data_dir, None)
    }

    #[cfg(test)]
    pub fn from_entries(entries: impl IntoIterator<Item = (String, TitleInfo)>) -> Self {
        let mut db = memory_database();
        let map = entries.into_iter().map(|(id, info)| (id.to_ascii_uppercase(), info)).collect();
        if let Err(error) = replace_titles(&mut db, &map) {
            panic!("in-memory TitleDB rejected fixtures: {error}");
        }
        Self {
            inner: Arc::new(Mutex::new(TitleDbInner {
                db,
                config: TitleDbConfig { enabled: false, ..Default::default() },
                data_dir: PathBuf::from("."),
                last_refresh: None,
                progress_tx: None,
            })),
            refreshing: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(1)),
        }
    }

    #[cfg(test)]
    fn from_artifacts(artifacts: &TitleDbArtifacts) -> Self {
        let mut db = memory_database();
        if let Err(error) = replace_artifacts(&mut db, artifacts) {
            panic!("in-memory TitleDB rejected artifacts: {error}");
        }
        Self {
            inner: Arc::new(Mutex::new(TitleDbInner {
                db,
                config: TitleDbConfig { enabled: false, ..Default::default() },
                data_dir: PathBuf::from("."),
                last_refresh: None,
                progress_tx: None,
            })),
            refreshing: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Create a `TitleDB` instance with optional progress broadcast channel.
    ///
    /// Progress messages are sent during refresh (e.g. for SSE in the admin UI).
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
        let mut db = open_database(&data_dir).unwrap_or_else(|error| {
            warn!(error = %error, "failed to open TitleDB SQLite cache; using memory");
            memory_database()
        });
        if config.enabled && !database_matches_config(&db, &config) {
            if let Err(error) = clear_database(&mut db) {
                warn!(error = %error, "failed to reset stale TitleDB SQLite cache");
            }
            if let Err(error) = import_json_cache(&mut db, &data_dir, &config) {
                warn!(error = %error, "failed to import legacy TitleDB cache");
            }
            if title_count(&db) > 0 {
                if let Err(error) = set_database_config(&db, &config) {
                    warn!(error = %error, "failed to record imported TitleDB configuration");
                }
            }
        }
        let initial_generation = u64::from(title_count(&db) > 0);
        Self {
            inner: Arc::new(Mutex::new(TitleDbInner {
                db,
                config,
                data_dir,
                last_refresh: None,
                progress_tx,
            })),
            refreshing: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(initial_generation)),
        }
    }

    #[allow(dead_code)]
    pub async fn progress_subscribe(&self) -> Option<broadcast::Receiver<String>> {
        self.inner.lock().await.progress_tx.as_ref().map(broadcast::Sender::subscribe)
    }

    /// Look up icon and banner URLs for a title ID (16-char hex, uppercase).
    pub async fn lookup(&self, title_id: &str) -> Option<TitleInfo> {
        let normalized = title_id.to_ascii_uppercase();
        let guard = self.inner.lock().await;
        let mut info = guard
            .db
            .query_row(
                "SELECT icon_url,banner_url,name,record FROM titles WHERE id=?1",
                [&normalized],
                |row| {
                    Ok(TitleInfo {
                        icon_url: row.get(0)?,
                        banner_url: row.get(1)?,
                        name: row.get(2)?,
                        record: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or_default(),
                    })
                },
            )
            .optional()
            .ok()
            .flatten();
        if let Ok(Some(raw)) = guard
            .db
            .query_row("SELECT record FROM custom_titles WHERE id=?1", [&normalized], |row| {
                row.get::<_, String>(0)
            })
            .optional()
        {
            if let Ok(record) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&raw)
            {
                let info = info.get_or_insert_with(TitleInfo::default);
                for (key, value) in record {
                    if !value.is_null() {
                        info.record.insert(key, value);
                    }
                }
                for (key, field) in [
                    ("name", &mut info.name),
                    ("iconUrl", &mut info.icon_url),
                    ("bannerUrl", &mut info.banner_url),
                ] {
                    if let Some(value) = info.record.get(key).and_then(serde_json::Value::as_str) {
                        *field = Some(value.to_string());
                    }
                }
            }
        }
        drop(guard);
        info
    }

    pub async fn set_override(
        &self,
        id: &str,
        record: Option<&serde_json::Value>,
    ) -> Result<(), TitleDbError> {
        let guard = self.inner.lock().await;
        if let Some(record) = record {
            guard.db.execute("INSERT INTO custom_titles(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![id,record.to_string()])?;
        } else {
            guard.db.execute("DELETE FROM custom_titles WHERE id=?1", [id])?;
        }
        drop(guard);
        self.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Return known update versions for a title or application ID.
    pub async fn versions(&self, title_id: &str) -> Option<TitleVersionInfo> {
        let normalized = normalize_title_id(title_id)?;
        let guard = self.inner.lock().await;
        guard
            .db
            .query_row(
                "SELECT latest_version, versions, release_dates FROM versions WHERE title_id = ?1",
                [&normalized],
                |row| {
                    let latest: Option<i64> = row.get(0)?;
                    let versions: String = row.get(1)?;
                    Ok((latest, versions, row.get::<_, String>(2)?))
                },
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|(latest, versions, dates)| {
                Some(TitleVersionInfo {
                    title_id: normalized,
                    latest_version: latest.and_then(|value| u64::try_from(value).ok()),
                    versions: serde_json::from_str(&versions).ok()?,
                    release_dates: serde_json::from_str(&dates).unwrap_or_default(),
                })
            })
    }

    /// Return the latest known update version for a title or application ID.
    pub async fn latest_version(&self, title_id: &str) -> Option<u64> {
        self.versions(title_id).await.and_then(|info| info.latest_version)
    }

    /// Return known CNMT metadata for a title/application/update/DLC ID.
    pub async fn cnmt(&self, app_id: &str) -> Option<TitleCnmtInfo> {
        let normalized = normalize_title_id(app_id)?;
        let guard = self.inner.lock().await;
        guard
            .db
            .query_row(
                "SELECT title_id, title_type, base_title_id, version, required_system_version \
                 FROM cnmts WHERE app_id = ?1 ORDER BY COALESCE(version, 0) DESC LIMIT 1",
                [normalized],
                cnmt_from_row,
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Return language metadata for a title/application ID.
    pub async fn languages(&self, title_id: &str) -> Option<TitleLanguageInfo> {
        let normalized = normalize_title_id(title_id)?;
        let guard = self.inner.lock().await;
        guard
            .db
            .query_row(
                "SELECT languages FROM languages WHERE title_id = ?1",
                [&normalized],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|languages| {
                Some(TitleLanguageInfo {
                    title_id: normalized,
                    languages: serde_json::from_str(&languages).ok()?,
                })
            })
    }

    pub async fn region_language_available(&self, region: &str, language: &str) -> Option<bool> {
        let guard = self.inner.lock().await;
        let languages = guard
            .db
            .query_row(
                "SELECT languages FROM regions WHERE region = ?1",
                [region.to_ascii_uppercase()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()?;
        drop(guard);
        serde_json::from_str::<Vec<String>>(&languages)
            .ok()
            .map(|known| known.iter().any(|value| value == language))
    }

    /// Return known update CNMT entries for the given base title ID.
    pub async fn updates_for_title(&self, title_id: &str) -> Vec<TitleCnmtInfo> {
        let Some(normalized) = normalize_title_id(title_id) else {
            return Vec::new();
        };
        let mut out = self.cnmts_for_base(&normalized).await;
        out.retain(|info| info.is_update_for(&normalized));
        out.sort_by(|a, b| a.title_id.cmp(&b.title_id));
        out
    }

    /// Return known DLC CNMT entries for the given base title ID.
    pub async fn dlc_for_title(&self, title_id: &str) -> Vec<TitleCnmtInfo> {
        let Some(normalized) = normalize_title_id(title_id) else {
            return Vec::new();
        };
        let mut out = self.cnmts_for_base(&normalized).await;
        out.retain(|info| info.is_dlc_for(&normalized));
        out.sort_by(|a, b| a.title_id.cmp(&b.title_id));
        out
    }

    /// Trigger a refresh. Returns immediately; refresh runs in background.
    /// Fetch runs without holding the lock so lookups remain fast during refresh.
    pub fn refresh(&self) {
        debug!("titledb refresh triggered");
        if self
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            debug!("titledb refresh skipped because another refresh is running");
            return;
        }
        let inner = Arc::clone(&self.inner);
        let refreshing = RefreshGuard(Arc::clone(&self.refreshing));
        let generation = Arc::clone(&self.generation);
        tokio::spawn(async move {
            match do_refresh_without_lock(&inner).await {
                Ok(()) => {
                    generation.fetch_add(1, Ordering::AcqRel);
                }
                Err(e) => error!(error = %e, "titledb refresh failed"),
            }
            drop(refreshing);
        });
    }

    pub async fn refresh_and_wait(&self) -> Result<(), TitleDbError> {
        while self
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let _refresh_guard = RefreshGuard(Arc::clone(&self.refreshing));
        let result = do_refresh_without_lock(&self.inner).await;
        if result.is_ok() {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        result
    }

    pub async fn config(&self) -> TitleDbConfig {
        self.inner.lock().await.config.clone()
    }

    pub async fn set_config(&self, config: TitleDbConfig) {
        self.inner.lock().await.config = config;
    }

    pub async fn last_refresh(&self) -> Option<std::time::Instant> {
        self.inner.lock().await.last_refresh
    }

    #[allow(clippy::significant_drop_tightening)] // SQLite statement borrows the connection guard through row iteration.
    pub async fn records(&self) -> Vec<serde_json::Value> {
        let guard = self.inner.lock().await;
        let Ok(mut statement) =
            guard.db.prepare("SELECT id,icon_url,banner_url,name,record FROM titles ORDER BY id")
        else {
            return Vec::new();
        };
        let Ok(rows) = statement.query_map([], |row| {
            let mut record = serde_json::from_str::<serde_json::Value>(&row.get::<_, String>(4)?)
                .unwrap_or_else(|_| serde_json::json!({}));
            record["titleId"] = row.get::<_, String>(0)?.into();
            record["iconUrl"] = row.get::<_, Option<String>>(1)?.into();
            record["bannerUrl"] = row.get::<_, Option<String>>(2)?.into();
            record["name"] = row.get::<_, Option<String>>(3)?.into();
            record["source"] = "titledb".into();
            Ok(record)
        }) else {
            return Vec::new();
        };
        rows.filter_map(Result::ok).collect()
    }

    pub async fn entry_count(&self) -> usize {
        title_count(&self.inner.lock().await.db)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    #[allow(clippy::significant_drop_tightening)] // SQLite statement borrows the connection guard through row iteration.
    async fn cnmts_for_base(&self, base_title_id: &str) -> Vec<TitleCnmtInfo> {
        let guard = self.inner.lock().await;
        let Ok(mut statement) = guard.db.prepare(
            "SELECT title_id, title_type, base_title_id, version, required_system_version \
             FROM cnmts WHERE base_title_id = ?1",
        ) else {
            return Vec::new();
        };
        statement
            .query_map([base_title_id], cnmt_from_row)
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }
}

fn send_progress(tx: Option<&broadcast::Sender<String>>, msg: &str) {
    if let Some(tx) = tx {
        let _ = tx.send(msg.to_string());
    }
}

/// Fetch and merge `TitleDB` data without holding the lock, then apply in a short write.
#[allow(clippy::too_many_lines)]
#[allow(clippy::cognitive_complexity)]
async fn do_refresh_without_lock(inner: &Mutex<TitleDbInner>) -> Result<(), TitleDbError> {
    let (enabled, region, lang, url_override, data_dir, progress_tx) = {
        let guard = inner.lock().await;
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

    send_progress(progress_tx.as_ref(), "[titledb] refresh starting");
    info!(
        region = %region,
        language = %lang,
        "titledb refresh starting"
    );

    let cache_path = data_dir.join("titledb").join(format!("titles.{region}.{lang}.json"));

    let parent = cache_path.parent().ok_or(TitleDbError::InvalidFormat)?;
    std::fs::create_dir_all(parent)?;
    debug!(cache_path = %cache_path.display(), "titledb cache path");

    send_progress(progress_tx.as_ref(), "[titledb] fetching from multiple sources...");

    // jsDelivr has a 20 MB limit for GitHub files; TitleDB JSON exceeds that
    let blawar_raw = Source::BlawarRaw {
        url: format!(
            "https://raw.githubusercontent.com/blawar/titledb/master/{region}.{lang}.json"
        ),
    };

    let sources: Vec<Source> =
        url_override.map_or_else(|| vec![blawar_raw], |url| vec![Source::OwnfoilZip { url }]);

    let merged = fetch_and_merge(&sources, &region, &lang, progress_tx.as_ref()).await?;

    send_progress(progress_tx.as_ref(), "[titledb] applying updates...");

    if merged.is_empty() {
        send_progress(progress_tx.as_ref(), "[titledb] network empty, trying cache...");
        info!("titledb network fetch returned no data, trying cache");
        let has_titles = title_count(&inner.lock().await.db) > 0;
        if !has_titles && cache_path.exists() {
            match load_cache(&cache_path) {
                Ok(loaded) => {
                    let count = loaded.len();
                    {
                        let mut guard = inner.lock().await;
                        replace_titles(&mut guard.db, &loaded)?;
                        guard.last_refresh = Some(std::time::Instant::now());
                    }
                    send_progress(
                        progress_tx.as_ref(),
                        &format!("[titledb] loaded {count} entries from cache"),
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
        } else if !has_titles {
            send_progress(progress_tx.as_ref(), "[titledb] empty, no cache available");
            warn!(
                path = %cache_path.display(),
                "titledb empty and no cache available"
            );
        }
    } else {
        let count = merged.len();
        {
            let mut guard = inner.lock().await;
            replace_titles(&mut guard.db, &merged)?;
            set_database_config(&guard.db, &guard.config)?;
            guard.last_refresh = Some(std::time::Instant::now());
        }
        send_progress(
            progress_tx.as_ref(),
            &format!("[titledb] loaded {count} entries from network"),
        );
        info!(entries = count, "titledb loaded from network");
    }
    drop(merged);

    match refresh_artifacts(&sources, parent, progress_tx.as_ref()).await {
        Ok(Some(artifacts)) => {
            let counts =
                (artifacts.versions.len(), artifacts.cnmts.len(), artifacts.languages.len());
            {
                let mut guard = inner.lock().await;
                replace_artifacts(&mut guard.db, &artifacts)?;
            }
            send_progress(
                progress_tx.as_ref(),
                &format!(
                    "[titledb] loaded artifacts: {} versions, {} cnmts, {} languages",
                    counts.0, counts.1, counts.2
                ),
            );
        }
        Ok(None) => {
            send_progress(progress_tx.as_ref(), "[titledb] no artifact metadata available");
        }
        Err(e) => {
            warn!(error = %e, "titledb artifact refresh failed");
        }
    }

    send_progress(progress_tx.as_ref(), "[titledb] refresh complete");
    Ok(())
}

async fn fetch_and_merge(
    sources: &[Source],
    region: &str,
    lang: &str,
    progress_tx: Option<&broadcast::Sender<String>>,
) -> Result<HashMap<String, TitleInfo>, TitleDbError> {
    let mut merged = HashMap::new();

    let handles: Vec<_> = sources.iter().map(|src| fetch_source(src, region, lang)).collect();

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
                send_progress(progress_tx, &format!("[titledb] {name} fetched {count} entries"));
                for (id, info) in entries {
                    merged
                        .entry(id.clone())
                        .or_insert_with(|| TitleInfo {
                            icon_url: None,
                            banner_url: None,
                            name: None,
                            record: serde_json::Map::default(),
                        })
                        .merge(info);
                }
                info!(source = %name, entries = count, "titledb source fetched");
            }
            Err(e) => {
                send_progress(progress_tx, &format!("[titledb] {name} failed: {e}"));
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
        .timeout(std::time::Duration::from_secs(180))
        .user_agent("ownfoil-rs/1.0 (TitleDB metadata fetcher)")
        .build()
        .unwrap_or_default()
}

const REMOTE_ZIP_BLOCK_SIZE: u64 = 8 * 1024 * 1024;

struct RemoteZipReader {
    client: reqwest::blocking::Client,
    url: String,
    len: u64,
    position: u64,
    cache_start: u64,
    cache: Vec<u8>,
}

impl RemoteZipReader {
    fn open(url: String) -> Result<Self, TitleDbError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .user_agent("ownfoil-rs/1.0 (TitleDB ranged ZIP reader)")
            .build()?;
        let response = client
            .get(&url)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()?
            .error_for_status()?;
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(TitleDbError::RangeUnsupported);
        }
        let len = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.rsplit_once('/'))
            .and_then(|(_, total)| total.parse().ok())
            .ok_or(TitleDbError::InvalidFormat)?;

        Ok(Self {
            client,
            url,
            len,
            position: 0,
            cache_start: 0,
            cache: response.bytes()?.to_vec(),
        })
    }

    fn refill(&mut self) -> std::io::Result<()> {
        let start = self.position;
        let end = start.saturating_add(REMOTE_ZIP_BLOCK_SIZE - 1).min(self.len - 1);
        let response = self
            .client
            .get(&self.url)
            .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(std::io::Error::other)?;
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(std::io::Error::other("TitleDB server ignored byte range"));
        }
        self.cache = response.bytes().map_err(std::io::Error::other)?.to_vec();
        self.cache_start = start;
        Ok(())
    }
}

impl Read for RemoteZipReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() || self.position >= self.len {
            return Ok(0);
        }
        let cache_end = self.cache_start.saturating_add(self.cache.len() as u64);
        if self.position < self.cache_start || self.position >= cache_end {
            self.refill()?;
        }
        let offset =
            usize::try_from(self.position - self.cache_start).map_err(std::io::Error::other)?;
        let available = self.cache.len().saturating_sub(offset);
        let remaining = usize::try_from(self.len - self.position).unwrap_or(usize::MAX);
        let count = output.len().min(available).min(remaining);
        output[..count].copy_from_slice(&self.cache[offset..offset + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for RemoteZipReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let position = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::End(offset) => i128::from(self.len) + i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
        };
        if !(0..=i128::from(self.len)).contains(&position) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek outside remote ZIP",
            ));
        }
        self.position = u64::try_from(position).map_err(std::io::Error::other)?;
        Ok(self.position)
    }
}

async fn fetch_ownfoil_zip(
    zip_url: &str,
    region: &str,
    lang: &str,
) -> Result<Vec<(String, TitleInfo)>, TitleDbError> {
    let zip_url = zip_url.to_string();
    let region = region.to_string();
    let lang = lang.to_string();
    tokio::task::spawn_blocking(move || fetch_ownfoil_zip_blocking(&zip_url, &region, &lang))
        .await
        .map_err(|_| TitleDbError::BackgroundTask)?
}

fn fetch_ownfoil_zip_blocking(
    zip_url: &str,
    region: &str,
    lang: &str,
) -> Result<Vec<(String, TitleInfo)>, TitleDbError> {
    let mut archive = zip::ZipArchive::new(RemoteZipReader::open(zip_url.to_string())?)?;

    let titles_name = format!("titles.{region}.{lang}.json");
    let alt_name = format!("{region}.{lang}.json");

    let has_titles = archive.by_name(&titles_name).is_ok();
    let mut file = if has_titles {
        #[allow(clippy::unwrap_used)]
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
    let resp = resp.error_for_status().map_err(|e| {
        warn!(
            url = %url,
            status = e.status().map_or(0, |status| status.as_u16()),
            "titledb blawar: HTTP error"
        );
        TitleDbError::Http(e)
    })?;
    let bytes = resp.bytes().await?;
    let buf = String::from_utf8(bytes.to_vec())?;
    parse_titles_json(&buf)
}

async fn refresh_artifacts(
    sources: &[Source],
    cache_dir: &std::path::Path,
    progress_tx: Option<&broadcast::Sender<String>>,
) -> Result<Option<TitleDbArtifacts>, TitleDbError> {
    send_progress(progress_tx, "[titledb] fetching artifact metadata...");

    for source in sources {
        match fetch_artifacts_from_source(source).await {
            Ok(artifacts) if !artifacts.is_empty() => {
                return Ok(Some(artifacts));
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "titledb artifact source failed"),
        }
    }

    match load_artifact_cache(cache_dir) {
        Ok(artifacts) if !artifacts.is_empty() => Ok(Some(artifacts)),
        Ok(_) => Ok(None),
        Err(e) => {
            warn!(error = %e, "titledb artifact cache load failed");
            Ok(None)
        }
    }
}

async fn fetch_artifacts_from_source(source: &Source) -> Result<TitleDbArtifacts, TitleDbError> {
    match source {
        Source::OwnfoilZip { url } => fetch_ownfoil_zip_artifacts(url).await,
        Source::BlawarRaw { url } => fetch_blawar_raw_artifacts(url).await,
    }
}

async fn fetch_blawar_raw_artifacts(url: &str) -> Result<TitleDbArtifacts, TitleDbError> {
    let base = url.rsplit_once('/').map_or(url, |(base, _)| base);
    let client = http_client();

    let versions_json = fetch_optional_text(&client, &format!("{base}/versions.json")).await?;
    let versions_txt = fetch_optional_text(&client, &format!("{base}/versions.txt")).await?;
    let cnmts_json = fetch_optional_text(&client, &format!("{base}/cnmts.json")).await?;
    let languages_json = fetch_optional_text(&client, &format!("{base}/languages.json")).await?;

    parse_artifact_buffers(
        versions_json.as_deref(),
        versions_txt.as_deref(),
        cnmts_json.as_deref(),
        languages_json.as_deref(),
    )
}

async fn fetch_optional_text(
    client: &reqwest::Client,
    url: &str,
) -> Result<Option<String>, TitleDbError> {
    let resp = client.get(url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let resp = resp.error_for_status()?;
    Ok(Some(String::from_utf8(resp.bytes().await?.to_vec())?))
}

async fn fetch_ownfoil_zip_artifacts(zip_url: &str) -> Result<TitleDbArtifacts, TitleDbError> {
    let zip_url = zip_url.to_string();
    tokio::task::spawn_blocking(move || fetch_ownfoil_zip_artifacts_blocking(zip_url))
        .await
        .map_err(|_| TitleDbError::BackgroundTask)?
}

fn fetch_ownfoil_zip_artifacts_blocking(zip_url: String) -> Result<TitleDbArtifacts, TitleDbError> {
    let mut archive = zip::ZipArchive::new(RemoteZipReader::open(zip_url)?)?;

    let versions_json = read_zip_text_optional(&mut archive, "versions.json")?;
    let versions_txt = read_zip_text_optional(&mut archive, "versions.txt")?;
    let cnmts_json = read_zip_text_optional(&mut archive, "cnmts.json")?;
    let languages_json = read_zip_text_optional(&mut archive, "languages.json")?;

    parse_artifact_buffers(
        versions_json.as_deref(),
        versions_txt.as_deref(),
        cnmts_json.as_deref(),
        languages_json.as_deref(),
    )
}

fn read_zip_text_optional<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Option<String>, TitleDbError> {
    match archive.by_name(name) {
        Ok(mut file) => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut file, &mut buf)?;
            Ok(Some(buf))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn parse_artifact_buffers(
    versions_json: Option<&str>,
    versions_txt: Option<&str>,
    cnmts_json: Option<&str>,
    languages_json: Option<&str>,
) -> Result<TitleDbArtifacts, TitleDbError> {
    let mut versions = HashMap::new();
    if let Some(buf) = versions_json {
        versions.extend(parse_versions_json(buf)?);
    }
    if let Some(buf) = versions_txt {
        for (id, info) in parse_versions_txt(buf) {
            versions
                .entry(id)
                .and_modify(|existing: &mut TitleVersionInfo| existing.merge_versions(&info))
                .or_insert(info);
        }
    }

    let cnmts = cnmts_json.map(parse_cnmts_json).transpose()?.unwrap_or_default();
    let languages = languages_json.map(parse_languages_json).transpose()?.unwrap_or_default();
    let regions = languages_json.map(parse_regions_json).transpose()?.unwrap_or_default();

    Ok(TitleDbArtifacts { versions, cnmts, languages, regions })
}

fn parse_titles_json(buf: &str) -> Result<Vec<(String, TitleInfo)>, TitleDbError> {
    let raw: serde_json::Value = serde_json::from_str(buf)?;
    let obj = raw.as_object().ok_or(TitleDbError::InvalidFormat)?;

    let mut out = Vec::new();
    for (_, v) in obj {
        let entry = v.as_object().ok_or(TitleDbError::InvalidFormat)?;
        let id = entry.get("id").and_then(|v| v.as_str()).map(str::to_uppercase);
        let Some(id) = id else { continue };
        if id.len() != 16 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }

        let mut icon_url = entry
            .get("iconUrl")
            .or_else(|| entry.get("icon_url"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        if let Some(ref url) = icon_url {
            if !url.is_empty() && !url.starts_with("http") {
                icon_url = Some(format!("https://img-eshop.cdn.nintendo.net{url}"));
            }
        }
        let banner_url = entry
            .get("bannerUrl")
            .or_else(|| entry.get("banner_url"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        let name = entry.get("name").and_then(|v| v.as_str()).map(ToString::to_string);

        out.push((id, TitleInfo { icon_url, banner_url, name, record: entry.clone() }));
    }

    Ok(out)
}

fn parse_versions_json(buf: &str) -> Result<HashMap<String, TitleVersionInfo>, TitleDbError> {
    let raw: serde_json::Value = serde_json::from_str(buf)?;
    let mut out = HashMap::new();

    match raw {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let Some(id) = normalize_title_id(&key).or_else(|| value_id(&value)) else {
                    continue;
                };
                let mut info = parse_version_value(&id, &value);
                info.title_id.clone_from(&id);
                out.insert(id, info);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                let Some(id) = value_id(&value) else {
                    continue;
                };
                let mut info = parse_version_value(&id, &value);
                info.title_id.clone_from(&id);
                out.insert(id, info);
            }
        }
        _ => return Err(TitleDbError::InvalidFormat),
    }

    Ok(out)
}

fn parse_versions_txt(buf: &str) -> HashMap<String, TitleVersionInfo> {
    let mut out = HashMap::new();
    for line in buf.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line
            .split(|c: char| c.is_ascii_whitespace() || matches!(c, ',' | ':' | '=' | '|'))
            .filter(|field| !field.is_empty())
            .collect();
        let Some(id) = fields.iter().find_map(|field| normalize_title_id(field)) else {
            continue;
        };
        let versions: Vec<u64> = fields
            .iter()
            .filter(|field| normalize_title_id(field).is_none())
            .filter_map(|field| parse_u64_value(field))
            .collect();
        if versions.is_empty() {
            continue;
        }
        let info = TitleVersionInfo::from_versions(id.clone(), versions);
        out.entry(id)
            .and_modify(|existing: &mut TitleVersionInfo| existing.merge_versions(&info))
            .or_insert(info);
    }
    out
}

fn parse_version_value(title_id: &str, value: &serde_json::Value) -> TitleVersionInfo {
    let versions = match value {
        serde_json::Value::Number(_) | serde_json::Value::String(_) => {
            value_u64(value).into_iter().collect()
        }
        serde_json::Value::Array(values) => values.iter().filter_map(value_u64).collect(),
        serde_json::Value::Object(map) => {
            let mut versions =
                map.keys().filter_map(|key| key.parse::<u64>().ok()).collect::<Vec<_>>();
            for key in ["version", "latest", "latest_version", "latestVersion"] {
                if let Some(version) = map.get(key).and_then(value_u64) {
                    versions.push(version);
                }
            }
            for key in ["versions", "versionList"] {
                if let Some(values) = map.get(key).and_then(|v| v.as_array()) {
                    versions.extend(values.iter().filter_map(value_u64));
                }
            }
            versions
        }
        _ => Vec::new(),
    };

    let mut info = TitleVersionInfo::from_versions(title_id.to_string(), versions);
    if let Some(map) = value.as_object() {
        for (version, metadata) in map {
            if let Ok(version) = version.parse::<u64>() {
                if let Some(date) = metadata
                    .as_str()
                    .or_else(|| metadata.get("releaseDate").and_then(serde_json::Value::as_str))
                {
                    info.release_dates.insert(version, date.to_string());
                }
            }
        }
    }
    info
}

fn parse_cnmts_json(buf: &str) -> Result<HashMap<String, Vec<TitleCnmtInfo>>, TitleDbError> {
    let raw: serde_json::Value = serde_json::from_str(buf)?;
    let values: Vec<(Option<String>, serde_json::Value)> = match raw {
        serde_json::Value::Object(map) => map.into_iter().map(|(k, v)| (Some(k), v)).collect(),
        serde_json::Value::Array(values) => values.into_iter().map(|v| (None, v)).collect(),
        _ => return Err(TitleDbError::InvalidFormat),
    };

    let mut out = HashMap::new();
    for (key, value) in values {
        let Some(title_id) =
            key.as_deref().and_then(normalize_title_id).or_else(|| value_id(&value))
        else {
            continue;
        };
        let Some(outer) = value.as_object() else {
            continue;
        };
        let direct = outer
            .keys()
            .any(|key| matches!(key.as_str(), "type" | "titleType" | "title_type" | "contentType"));
        let mut entries = Vec::new();
        if direct {
            entries.push(cnmt_info(&title_id, None, outer));
        } else {
            for (version, metadata) in outer {
                if let Some(metadata) = metadata.as_object() {
                    entries.push(cnmt_info(&title_id, parse_u64_value(version), metadata));
                }
            }
        }
        entries.sort_by_key(|info| info.version.unwrap_or(0));
        if !entries.is_empty() {
            out.insert(title_id, entries);
        }
    }

    Ok(out)
}

fn cnmt_info(
    title_id: &str,
    version_from_key: Option<u64>,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> TitleCnmtInfo {
    let title_type = obj
        .get("titleType")
        .and_then(value_u64)
        .map(|kind| match kind {
            128 => "BASE".to_string(),
            129 => "UPDATE".to_string(),
            130 => "DLC".to_string(),
            other => other.to_string(),
        })
        .or_else(|| string_field(obj, &["type", "titleType", "title_type", "contentType"]));
    let base_title_id = string_field(
        obj,
        &[
            "otherApplicationId",
            "baseId",
            "base_id",
            "baseTitleId",
            "base_title_id",
            "applicationId",
            "application_id",
        ],
    )
    .and_then(|id| normalize_title_id(&id));
    let version =
        version_from_key.or_else(|| u64_field(obj, &["version", "titleVersion", "title_version"]));
    let required_system_version = u64_field(
        obj,
        &["requiredSystemVersion", "required_system_version", "requiredDownloadSystemVersion"],
    );
    TitleCnmtInfo {
        title_id: title_id.to_string(),
        title_type,
        base_title_id,
        version,
        required_system_version,
    }
}

fn parse_languages_json(buf: &str) -> Result<HashMap<String, TitleLanguageInfo>, TitleDbError> {
    let raw: serde_json::Value = serde_json::from_str(buf)?;
    let values: Vec<(Option<String>, serde_json::Value)> = match raw {
        serde_json::Value::Object(map) => map.into_iter().map(|(k, v)| (Some(k), v)).collect(),
        serde_json::Value::Array(values) => values.into_iter().map(|v| (None, v)).collect(),
        _ => return Err(TitleDbError::InvalidFormat),
    };

    let mut out = HashMap::new();
    for (key, value) in values {
        let Some(title_id) =
            key.as_deref().and_then(normalize_title_id).or_else(|| value_id(&value))
        else {
            continue;
        };
        let languages = language_values(&value);
        if languages.is_empty() {
            continue;
        }
        out.insert(title_id.clone(), TitleLanguageInfo { title_id, languages });
    }

    Ok(out)
}

fn parse_regions_json(buf: &str) -> Result<HashMap<String, Vec<String>>, TitleDbError> {
    let raw: serde_json::Value = serde_json::from_str(buf)?;
    let object = raw.as_object().ok_or(TitleDbError::InvalidFormat)?;
    let mut regions = HashMap::new();
    for (region, value) in object {
        if region.len() == 16 || region.len() > 4 {
            continue;
        }
        let languages = language_values(value);
        if !languages.is_empty() {
            regions.insert(region.to_ascii_uppercase(), languages);
        }
    }
    Ok(regions)
}

impl TitleInfo {
    fn merge(&mut self, other: &Self) {
        for (key, value) in &other.record {
            self.record.entry(key).or_insert_with(|| value.clone());
        }
        if self.icon_url.is_none() && other.icon_url.is_some() {
            self.icon_url.clone_from(&other.icon_url);
        }
        if self.banner_url.is_none() && other.banner_url.is_some() {
            self.banner_url.clone_from(&other.banner_url);
        }
        if self.name.is_none() && other.name.is_some() {
            self.name.clone_from(&other.name);
        }
    }
}

impl TitleDbArtifacts {
    fn is_empty(&self) -> bool {
        self.versions.is_empty()
            && self.cnmts.is_empty()
            && self.languages.is_empty()
            && self.regions.is_empty()
    }
}

impl TitleVersionInfo {
    fn from_versions(title_id: String, mut versions: Vec<u64>) -> Self {
        versions.sort_unstable();
        versions.dedup();
        let latest_version = versions.iter().copied().max();
        Self {
            title_id,
            latest_version,
            versions,
            release_dates: std::collections::BTreeMap::default(),
        }
    }

    fn merge_versions(&mut self, other: &Self) {
        self.release_dates.extend(other.release_dates.clone());
        self.versions.extend(other.versions.iter().copied());
        self.versions.sort_unstable();
        self.versions.dedup();
        self.latest_version = self
            .latest_version
            .into_iter()
            .chain(other.latest_version)
            .chain(self.versions.iter().copied())
            .max();
    }
}

#[allow(dead_code)]
impl TitleCnmtInfo {
    fn is_update_for(&self, base_title_id: &str) -> bool {
        if self.base_title_id.as_deref() == Some(base_title_id) && self.has_type("update") {
            return true;
        }
        let Some(update_id) = update_title_id(base_title_id) else {
            return false;
        };
        self.title_id == update_id
    }

    fn is_dlc_for(&self, base_title_id: &str) -> bool {
        self.base_title_id.as_deref() == Some(base_title_id) && self.has_type("add")
    }

    fn has_type(&self, needle: &str) -> bool {
        self.title_type.as_deref().is_some_and(|value| value.to_ascii_lowercase().contains(needle))
    }
}

fn normalize_title_id(value: &str) -> Option<String> {
    let id = value.trim().trim_start_matches("0x").trim_start_matches("0X");
    if id.len() == 16 && id.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(id.to_ascii_uppercase())
    } else {
        None
    }
}

#[allow(dead_code)]
fn update_title_id(base_title_id: &str) -> Option<String> {
    let id = u64::from_str_radix(base_title_id, 16).ok()?;
    Some(format!("{:016X}", id | 0x800))
}

fn value_id(value: &serde_json::Value) -> Option<String> {
    value.as_object().and_then(|obj| {
        string_field(obj, &["id", "titleId", "title_id", "appId", "app_id"])
            .and_then(|id| normalize_title_id(&id))
    })
}

fn string_field(obj: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| obj.get(*key))
        .and_then(|value| value.as_str().map(ToString::to_string))
}

fn u64_field(obj: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| obj.get(*key).and_then(value_u64))
}

fn value_u64(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(raw) => parse_u64_value(raw),
        _ => None,
    }
}

fn parse_u64_value(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    raw.strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .map_or_else(|| raw.parse().ok(), |hex| u64::from_str_radix(hex, 16).ok())
}

fn language_values(value: &serde_json::Value) -> Vec<String> {
    let mut values = match value {
        serde_json::Value::Array(values) => {
            values.iter().filter_map(|value| value.as_str().map(ToString::to_string)).collect()
        }
        serde_json::Value::String(value) => vec![value.clone()],
        serde_json::Value::Object(obj) => obj
            .get("languages")
            .or_else(|| obj.get("language"))
            .or_else(|| obj.get("langs"))
            .map(language_values)
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    values.sort();
    values.dedup();
    values
}

fn memory_database() -> Connection {
    let db = Connection::open_in_memory()
        .unwrap_or_else(|error| panic!("failed to open in-memory SQLite database: {error}"));
    initialize_database(&db)
        .unwrap_or_else(|error| panic!("failed to initialize in-memory SQLite schema: {error}"));
    db
}

fn open_database(data_dir: &std::path::Path) -> Result<Connection, TitleDbError> {
    let cache_dir = data_dir.join("titledb");
    std::fs::create_dir_all(&cache_dir)?;
    let db = Connection::open(cache_dir.join("metadata.sqlite"))?;
    initialize_database(&db)?;
    Ok(db)
}

fn initialize_database(db: &Connection) -> Result<(), rusqlite::Error> {
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA cache_size=-2048;
         CREATE TABLE IF NOT EXISTS titles (
           id TEXT PRIMARY KEY, icon_url TEXT, banner_url TEXT, name TEXT, record TEXT NOT NULL DEFAULT '{}'
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS custom_titles (id TEXT PRIMARY KEY,record TEXT NOT NULL) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS versions (
           title_id TEXT PRIMARY KEY, latest_version INTEGER, versions TEXT NOT NULL, release_dates TEXT NOT NULL DEFAULT '{}'
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS cnmts (
           app_id TEXT NOT NULL, title_id TEXT NOT NULL, title_type TEXT,
           base_title_id TEXT, version INTEGER, required_system_version INTEGER
         );
         CREATE INDEX IF NOT EXISTS cnmts_app_id ON cnmts(app_id);
         CREATE INDEX IF NOT EXISTS cnmts_base_title_id ON cnmts(base_title_id);
         CREATE TABLE IF NOT EXISTS languages (
           title_id TEXT PRIMARY KEY, languages TEXT NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS regions (
           region TEXT PRIMARY KEY, languages TEXT NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS metadata (
           key TEXT PRIMARY KEY, value TEXT NOT NULL
         ) WITHOUT ROWID;",
    )?;
    let mut columns = db.prepare("PRAGMA table_info(titles)")?;
    let names = columns.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<Vec<_>, _>>()?;
    if !names.iter().any(|name| name == "record") {
        db.execute("ALTER TABLE titles ADD COLUMN record TEXT NOT NULL DEFAULT '{}'", [])?;
    }
    let mut columns = db.prepare("PRAGMA table_info(versions)")?;
    let names = columns.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<Vec<_>, _>>()?;
    if !names.iter().any(|name| name == "release_dates") {
        db.execute("ALTER TABLE versions ADD COLUMN release_dates TEXT NOT NULL DEFAULT '{}'", [])?;
    }
    Ok(())
}

fn import_json_cache(
    db: &mut Connection,
    data_dir: &std::path::Path,
    config: &TitleDbConfig,
) -> Result<(), TitleDbError> {
    let cache_dir = data_dir.join("titledb");
    let titles_path = cache_dir.join(format!("titles.{}.{}.json", config.region, config.language));
    if titles_path.exists() {
        let titles = load_cache(&titles_path)?;
        replace_titles(db, &titles)?;
    }
    let artifacts = load_artifact_cache(&cache_dir)?;
    if !artifacts.is_empty() {
        replace_artifacts(db, &artifacts)?;
    }
    Ok(())
}

fn database_matches_config(db: &Connection, config: &TitleDbConfig) -> bool {
    let value = |key: &str| {
        db.query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
    };
    title_count(db) > 0
        && value("region").as_deref() == Some(config.region.as_str())
        && value("language").as_deref() == Some(config.language.as_str())
}

fn set_database_config(db: &Connection, config: &TitleDbConfig) -> Result<(), TitleDbError> {
    db.execute(
        "INSERT OR REPLACE INTO metadata (key, value) VALUES ('region', ?1)",
        [&config.region],
    )?;
    db.execute(
        "INSERT OR REPLACE INTO metadata (key, value) VALUES ('language', ?1)",
        [&config.language],
    )?;
    Ok(())
}

fn clear_database(db: &mut Connection) -> Result<(), TitleDbError> {
    let transaction = db.transaction()?;
    transaction.execute_batch(
        "DELETE FROM titles; DELETE FROM versions; DELETE FROM cnmts; DELETE FROM languages; \
         DELETE FROM regions; DELETE FROM metadata;",
    )?;
    transaction.commit()?;
    Ok(())
}

fn replace_titles(
    db: &mut Connection,
    titles: &HashMap<String, TitleInfo>,
) -> Result<(), TitleDbError> {
    let transaction = db.transaction()?;
    transaction.execute("DELETE FROM titles", [])?;
    {
        let mut insert = transaction.prepare(
            "INSERT INTO titles (id, icon_url, banner_url, name, record) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (id, info) in titles {
            insert.execute(params![
                id,
                info.icon_url,
                info.banner_url,
                info.name,
                serde_json::to_string(&info.record)?
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn replace_artifacts(
    db: &mut Connection,
    artifacts: &TitleDbArtifacts,
) -> Result<(), TitleDbError> {
    let transaction = db.transaction()?;
    transaction.execute_batch(
        "DELETE FROM versions; DELETE FROM cnmts; DELETE FROM languages; DELETE FROM regions;",
    )?;
    {
        let mut insert = transaction.prepare(
            "INSERT INTO versions (title_id, latest_version, versions, release_dates) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for info in artifacts.versions.values() {
            insert.execute(params![
                info.title_id,
                info.latest_version.and_then(|value| i64::try_from(value).ok()),
                serde_json::to_string(&info.versions)?,
                serde_json::to_string(&info.release_dates)?
            ])?;
        }
    }
    {
        let mut insert = transaction.prepare(
            "INSERT INTO cnmts (app_id, title_id, title_type, base_title_id, version, \
             required_system_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (app_id, entries) in &artifacts.cnmts {
            for info in entries {
                insert.execute(params![
                    app_id,
                    info.title_id,
                    info.title_type,
                    info.base_title_id,
                    info.version.and_then(|value| i64::try_from(value).ok()),
                    info.required_system_version.and_then(|value| i64::try_from(value).ok())
                ])?;
            }
        }
    }
    {
        let mut insert =
            transaction.prepare("INSERT INTO languages (title_id, languages) VALUES (?1, ?2)")?;
        for info in artifacts.languages.values() {
            insert.execute(params![info.title_id, serde_json::to_string(&info.languages)?])?;
        }
    }
    {
        let mut insert =
            transaction.prepare("INSERT INTO regions (region, languages) VALUES (?1, ?2)")?;
        for (region, languages) in &artifacts.regions {
            insert.execute(params![region, serde_json::to_string(languages)?])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn title_count(db: &Connection) -> usize {
    db.query_row("SELECT COUNT(*) FROM titles", [], |row| row.get::<_, i64>(0))
        .ok()
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(0)
}

fn cnmt_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TitleCnmtInfo> {
    let version: Option<i64> = row.get(3)?;
    let required_system_version: Option<i64> = row.get(4)?;
    Ok(TitleCnmtInfo {
        title_id: row.get(0)?,
        title_type: row.get(1)?,
        base_title_id: row.get(2)?,
        version: version.and_then(|value| u64::try_from(value).ok()),
        required_system_version: required_system_version
            .and_then(|value| u64::try_from(value).ok()),
    })
}

fn load_cache(path: &std::path::Path) -> Result<HashMap<String, TitleInfo>, TitleDbError> {
    let buf = std::fs::read_to_string(path)?;
    let raw: serde_json::Value = serde_json::from_str(&buf)?;
    if raw.is_object() {
        return Ok(parse_titles_json(&buf)?.into_iter().collect());
    }
    let raw = raw.as_array().ok_or(TitleDbError::InvalidFormat)?;
    let mut map = HashMap::new();
    for v in raw {
        let obj = v.as_object().ok_or(TitleDbError::InvalidFormat)?;
        let id = obj.get("id").and_then(|v| v.as_str()).map(str::to_uppercase);
        let Some(id) = id else { continue };
        let icon_url = obj.get("icon_url").and_then(|v| v.as_str()).map(String::from);
        let banner_url = obj.get("banner_url").and_then(|v| v.as_str()).map(String::from);
        let name = obj.get("name").and_then(|v| v.as_str()).map(String::from);
        map.insert(
            id,
            TitleInfo {
                icon_url,
                banner_url,
                name,
                record: obj
                    .get("record")
                    .and_then(serde_json::Value::as_object)
                    .cloned()
                    .unwrap_or_else(|| obj.clone()),
            },
        );
    }
    Ok(map)
}

fn load_artifact_cache(cache_dir: &std::path::Path) -> Result<TitleDbArtifacts, TitleDbError> {
    let versions = load_optional_artifact(cache_dir, "versions.json", parse_versions_json)?;
    let versions_txt = load_optional_artifact_txt(cache_dir, "versions.txt");
    let cnmts = load_optional_artifact(cache_dir, "cnmts.json", parse_cnmts_json)?;
    let languages = load_optional_artifact(cache_dir, "languages.json", parse_languages_json)?;
    let regions = load_optional_artifact(cache_dir, "languages.json", parse_regions_json)?;

    let mut versions = versions.unwrap_or_default();
    for (id, info) in versions_txt {
        versions.entry(id).and_modify(|existing| existing.merge_versions(&info)).or_insert(info);
    }

    Ok(TitleDbArtifacts {
        versions,
        cnmts: cnmts.unwrap_or_default(),
        languages: languages.unwrap_or_default(),
        regions: regions.unwrap_or_default(),
    })
}

fn load_optional_artifact<T>(
    cache_dir: &std::path::Path,
    name: &str,
    parser: fn(&str) -> Result<T, TitleDbError>,
) -> Result<Option<T>, TitleDbError> {
    let path = cache_dir.join(name);
    if !path.exists() {
        return Ok(None);
    }
    let buf = std::fs::read_to_string(path)?;
    parser(&buf).map(Some)
}

fn load_optional_artifact_txt(
    cache_dir: &std::path::Path,
    name: &str,
) -> HashMap<String, TitleVersionInfo> {
    let path = cache_dir.join(name);
    if !path.exists() {
        return HashMap::new();
    }
    std::fs::read_to_string(path).map_or_else(|_| HashMap::new(), |buf| parse_versions_txt(&buf))
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
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("remote ZIP server does not support byte ranges")]
    RangeUnsupported,
    #[error("TitleDB background task failed")]
    BackgroundTask,
    #[error("invalid format")]
    InvalidFormat,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_json_and_txt() {
        let json = r#"{
            "0100000000010000": {"versions": [0, "65536"], "latestVersion": 131072},
            "0100000000020000": "42"
        }"#;
        let txt = "0100000000010000 262144\n0100000000030000|7\n";

        let mut versions = parse_versions_json(json).expect("versions json parses");
        for (id, info) in parse_versions_txt(txt) {
            versions
                .entry(id)
                .and_modify(|existing| existing.merge_versions(&info))
                .or_insert(info);
        }

        let first = versions.get("0100000000010000").expect("first title");
        assert_eq!(first.latest_version, Some(262_144));
        assert_eq!(first.versions, vec![0, 65_536, 131_072, 262_144]);

        assert_eq!(versions.get("0100000000020000").and_then(|info| info.latest_version), Some(42));
        assert_eq!(versions.get("0100000000030000").and_then(|info| info.latest_version), Some(7));
    }

    #[test]
    fn parses_cnmt_and_language_artifacts() {
        let cnmts = r#"{
            "0100000000010800": {
                "type": "Patch",
                "baseId": "0100000000010000",
                "version": "65536",
                "requiredSystemVersion": "0x100"
            },
            "0100000000011001": {
                "titleType": "AddOnContent",
                "baseTitleId": "0100000000010000",
                "version": 3
            }
        }"#;
        let languages = r#"{
            "0100000000010000": ["en", "ja", "en"],
            "0100000000020000": {"languages": ["fr", "de"]}
        }"#;

        let cnmts = parse_cnmts_json(cnmts).expect("cnmts parse");
        let update =
            cnmts.get("0100000000010800").and_then(|items| items.first()).expect("update cnmt");
        assert!(update.is_update_for("0100000000010000"));
        assert_eq!(update.version, Some(65_536));
        assert_eq!(update.required_system_version, Some(256));

        let dlc = cnmts.get("0100000000011001").and_then(|items| items.first()).expect("dlc cnmt");
        assert!(dlc.is_dlc_for("0100000000010000"));

        let languages = parse_languages_json(languages).expect("languages parse");
        assert_eq!(
            languages.get("0100000000010000").expect("language entry").languages,
            vec!["en".to_string(), "ja".to_string()]
        );
    }

    #[test]
    fn parses_upstream_nested_cnmt_versions_and_regions() {
        let cnmts = parse_cnmts_json(
            r#"{"0100000000010800":{"0":{"titleType":129,"otherApplicationId":"0100000000010000"},"65536":{"titleType":129,"otherApplicationId":"0100000000010000"}}}"#,
        )
        .expect("nested CNMT parses");
        let updates = cnmts.get("0100000000010800").expect("app exists");
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[1].version, Some(65_536));
        assert!(updates[1].is_update_for("0100000000010000"));

        let regions = parse_regions_json(r#"{"US":["en","fr"],"GB":["en"]}"#)
            .expect("region languages parse");
        assert_eq!(regions.get("US"), Some(&vec!["en".to_string(), "fr".to_string()]));
    }

    #[tokio::test]
    async fn titledb_artifact_lookup_methods_return_metadata() {
        let artifacts = parse_artifact_buffers(
            Some(r#"{"0100000000010000": [0, 65536]}"#),
            Some("0100000000010000 131072"),
            Some(
                r#"{
                "0100000000010800": {"type": "Patch", "baseId": "0100000000010000", "version": 131072},
                    "0100000000011001": {"type": "AddOnContent", "baseId": "0100000000010000", "version": 1}
                }"#,
            ),
            Some(r#"{"0100000000010000": {"languages": ["en", "it"]}}"#),
        )
        .expect("artifact fixtures parse");
        let titledb = TitleDb::from_artifacts(&artifacts);

        assert_eq!(titledb.latest_version("0100000000010000").await, Some(131_072));
        assert_eq!(
            titledb.cnmt("0100000000010800").await.and_then(|info| info.version),
            Some(131_072)
        );
        assert_eq!(titledb.updates_for_title("0100000000010000").await.len(), 1);
        assert_eq!(titledb.dlc_for_title("0100000000010000").await.len(), 1);
        assert_eq!(
            titledb.languages("0100000000010000").await.expect("languages").languages,
            vec!["en".to_string(), "it".to_string()]
        );
    }

    #[tokio::test]
    async fn sqlite_cache_is_disabled_imported_once_and_region_scoped() {
        let dir = tempfile::tempdir().expect("temp directory");
        let cache_dir = dir.path().join("titledb");
        std::fs::create_dir_all(&cache_dir).expect("cache directory");
        std::fs::write(
            cache_dir.join("titles.US.en.json"),
            r#"{"0100000000010000":{"id":"0100000000010000","name":"US title","iconUrl":"https://example.test/us.jpg"}}"#,
        )
        .expect("US fixture");

        let disabled = TitleDb::with_progress(
            TitleDbConfig { enabled: false, ..TitleDbConfig::default() },
            dir.path().to_path_buf(),
            None,
        );
        assert_eq!(disabled.entry_count().await, 0);
        drop(disabled);

        let enabled =
            TitleDb::with_progress(TitleDbConfig::default(), dir.path().to_path_buf(), None);
        assert_eq!(enabled.entry_count().await, 1);
        assert_eq!(
            enabled.lookup("0100000000010000").await.and_then(|info| info.name),
            Some("US title".to_string())
        );
        drop(enabled);
        std::fs::remove_file(cache_dir.join("titles.US.en.json")).expect("remove legacy cache");

        let reopened =
            TitleDb::with_progress(TitleDbConfig::default(), dir.path().to_path_buf(), None);
        assert_eq!(reopened.entry_count().await, 1);
        drop(reopened);

        std::fs::write(
            cache_dir.join("titles.EU.fr.json"),
            r#"{"0100000000020000":{"id":"0100000000020000","name":"EU title"}}"#,
        )
        .expect("EU fixture");
        let european = TitleDb::with_progress(
            TitleDbConfig {
                region: "EU".to_string(),
                language: "fr".to_string(),
                ..TitleDbConfig::default()
            },
            dir.path().to_path_buf(),
            None,
        );
        assert_eq!(european.entry_count().await, 1);
        assert!(european.lookup("0100000000010000").await.is_none());
        assert_eq!(
            european.lookup("0100000000020000").await.and_then(|info| info.name),
            Some("EU title".to_string())
        );
    }
}
