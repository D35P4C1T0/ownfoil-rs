//! `TitleDB` integration: fetch game metadata (icon/banner URLs) from multiple sources.
//! Fetches concurrently from all sources and merges results redundantly.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{RwLock, broadcast};
use tracing::{debug, error, info, warn};

use crate::config::TitleDbConfig;

/// Per-title metadata from `TitleDB`.
///
/// Used to enrich shop section items with icon/banner URLs and display names.
#[derive(Debug, Clone)]
pub struct TitleInfo {
    /// CDN URL for the game icon (e.g. Nintendo eShop).
    pub icon_url: Option<String>,
    /// CDN URL for the banner image.
    pub banner_url: Option<String>,
    /// Localized game name.
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleVersionInfo {
    pub title_id: String,
    pub latest_version: Option<u64>,
    pub versions: Vec<u64>,
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
    cnmts: HashMap<String, TitleCnmtInfo>,
    languages: HashMap<String, TitleLanguageInfo>,
}

/// Lazy-loaded `TitleDB` cache. Loads from disk on first access, refreshes in background.
#[derive(Debug, Clone)]
pub struct TitleDb {
    inner: Arc<RwLock<TitleDbInner>>,
}

#[derive(Debug)]
struct TitleDbInner {
    map: HashMap<String, TitleInfo>,
    artifacts: TitleDbArtifacts,
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
        Self {
            inner: Arc::new(RwLock::new(TitleDbInner {
                map: entries
                    .into_iter()
                    .map(|(id, info)| (id.to_ascii_uppercase(), info))
                    .collect(),
                artifacts: TitleDbArtifacts::default(),
                config: TitleDbConfig { enabled: false, ..Default::default() },
                data_dir: PathBuf::from("."),
                last_refresh: None,
                progress_tx: None,
            })),
        }
    }

    #[cfg(test)]
    fn from_artifacts(artifacts: TitleDbArtifacts) -> Self {
        Self {
            inner: Arc::new(RwLock::new(TitleDbInner {
                map: HashMap::new(),
                artifacts,
                config: TitleDbConfig { enabled: false, ..Default::default() },
                data_dir: PathBuf::from("."),
                last_refresh: None,
                progress_tx: None,
            })),
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
        Self {
            inner: Arc::new(RwLock::new(TitleDbInner {
                map: HashMap::new(),
                artifacts: TitleDbArtifacts::default(),
                config,
                data_dir,
                last_refresh: None,
                progress_tx,
            })),
        }
    }

    #[allow(dead_code)]
    pub async fn progress_subscribe(&self) -> Option<broadcast::Receiver<String>> {
        self.inner.read().await.progress_tx.as_ref().map(broadcast::Sender::subscribe)
    }

    /// Look up icon and banner URLs for a title ID (16-char hex, uppercase).
    pub async fn lookup(&self, title_id: &str) -> Option<TitleInfo> {
        let normalized = title_id.to_uppercase();
        let guard = self.inner.read().await;
        guard.map.get(&normalized).cloned()
    }

    /// Return known update versions for a title or application ID.
    pub async fn versions(&self, title_id: &str) -> Option<TitleVersionInfo> {
        let normalized = normalize_title_id(title_id)?;
        let guard = self.inner.read().await;
        guard.artifacts.versions.get(&normalized).cloned()
    }

    /// Return the latest known update version for a title or application ID.
    pub async fn latest_version(&self, title_id: &str) -> Option<u64> {
        self.versions(title_id).await.and_then(|info| info.latest_version)
    }

    /// Return known CNMT metadata for a title/application/update/DLC ID.
    pub async fn cnmt(&self, app_id: &str) -> Option<TitleCnmtInfo> {
        let normalized = normalize_title_id(app_id)?;
        let guard = self.inner.read().await;
        guard.artifacts.cnmts.get(&normalized).cloned()
    }

    /// Return language metadata for a title/application ID.
    pub async fn languages(&self, title_id: &str) -> Option<TitleLanguageInfo> {
        let normalized = normalize_title_id(title_id)?;
        let guard = self.inner.read().await;
        guard.artifacts.languages.get(&normalized).cloned()
    }

    /// Return known update CNMT entries for the given base title ID.
    pub async fn updates_for_title(&self, title_id: &str) -> Vec<TitleCnmtInfo> {
        let Some(normalized) = normalize_title_id(title_id) else {
            return Vec::new();
        };
        let mut out: Vec<_> = {
            let guard = self.inner.read().await;
            guard
                .artifacts
                .cnmts
                .values()
                .filter(|info| info.is_update_for(&normalized))
                .cloned()
                .collect()
        };
        out.sort_by(|a, b| a.title_id.cmp(&b.title_id));
        out
    }

    /// Return known DLC CNMT entries for the given base title ID.
    pub async fn dlc_for_title(&self, title_id: &str) -> Vec<TitleCnmtInfo> {
        let Some(normalized) = normalize_title_id(title_id) else {
            return Vec::new();
        };
        let mut out: Vec<_> = {
            let guard = self.inner.read().await;
            guard
                .artifacts
                .cnmts
                .values()
                .filter(|info| info.is_dlc_for(&normalized))
                .cloned()
                .collect()
        };
        out.sort_by(|a, b| a.title_id.cmp(&b.title_id));
        out
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

fn send_progress(tx: Option<&broadcast::Sender<String>>, msg: &str) {
    if let Some(tx) = tx {
        let _ = tx.send(msg.to_string());
    }
}

/// Fetch and merge `TitleDB` data without holding the lock, then apply in a short write.
#[allow(clippy::too_many_lines)]
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

    send_progress(progress_tx.as_ref(), "[titledb] refresh starting");
    info!(
        region = %region,
        language = %lang,
        "titledb refresh starting"
    );

    let cache_path = data_dir.join("titledb").join(format!("{region}.{lang}.json"));

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

    let mut sources: Vec<Source> = vec![blawar_raw];
    if let Some(url) = url_override {
        sources.push(Source::OwnfoilZip { url });
    }

    let merged = fetch_and_merge(&sources, &region, &lang, progress_tx.as_ref()).await?;

    send_progress(progress_tx.as_ref(), "[titledb] applying updates...");

    if merged.is_empty() {
        send_progress(progress_tx.as_ref(), "[titledb] network empty, trying cache...");
        info!("titledb network fetch returned no data, trying cache");
        if cache_path.exists() {
            match load_cache(&cache_path) {
                Ok(loaded) => {
                    let count = loaded.len();
                    {
                        let mut guard = inner.write().await;
                        guard.map = loaded;
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
        } else {
            send_progress(progress_tx.as_ref(), "[titledb] empty, no cache available");
            warn!(
                path = %cache_path.display(),
                "titledb empty and no cache available"
            );
        }
    } else {
        let count = merged.len();
        if let Err(e) = save_cache(&cache_path, &merged) {
            warn!(path = %cache_path.display(), error = %e, "titledb cache save failed");
        } else {
            send_progress(progress_tx.as_ref(), "[titledb] cache saved");
            debug!(path = %cache_path.display(), "titledb cache saved");
        }
        {
            let mut guard = inner.write().await;
            guard.map = merged;
            guard.last_refresh = Some(std::time::Instant::now());
        }
        send_progress(
            progress_tx.as_ref(),
            &format!("[titledb] loaded {count} entries from network"),
        );
        info!(entries = count, "titledb loaded from network");
    }

    match refresh_artifacts(&sources, parent, progress_tx.as_ref()).await {
        Ok(Some(artifacts)) => {
            let counts =
                (artifacts.versions.len(), artifacts.cnmts.len(), artifacts.languages.len());
            {
                let mut guard = inner.write().await;
                guard.artifacts = artifacts;
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
    let resp = resp.error_for_status().map_err(|e| {
        warn!(
            url = %zip_url,
            status = e.status().map_or(0, |status| status.as_u16()),
            "titledb ownfoil zip: HTTP error"
        );
        TitleDbError::Http(e)
    })?;
    let bytes = resp.bytes().await?;
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)?;

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
                save_artifact_cache(cache_dir, &artifacts)?;
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
    let client = http_client();
    let resp = client.get(zip_url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)?;

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

    Ok(TitleDbArtifacts { versions, cnmts, languages })
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

        out.push((id, TitleInfo { icon_url, banner_url, name }));
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
            let mut versions = Vec::new();
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

    TitleVersionInfo::from_versions(title_id.to_string(), versions)
}

fn parse_cnmts_json(buf: &str) -> Result<HashMap<String, TitleCnmtInfo>, TitleDbError> {
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
        let Some(obj) = value.as_object() else {
            continue;
        };
        let title_type = string_field(obj, &["type", "titleType", "title_type", "contentType"]);
        let base_title_id = string_field(
            obj,
            &[
                "baseId",
                "base_id",
                "baseTitleId",
                "base_title_id",
                "applicationId",
                "application_id",
            ],
        )
        .and_then(|id| normalize_title_id(&id));
        let version = u64_field(obj, &["version", "titleVersion", "title_version"]);
        let required_system_version = u64_field(
            obj,
            &["requiredSystemVersion", "required_system_version", "requiredDownloadSystemVersion"],
        );

        out.insert(
            title_id.clone(),
            TitleCnmtInfo { title_id, title_type, base_title_id, version, required_system_version },
        );
    }

    Ok(out)
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

impl TitleInfo {
    fn merge(&mut self, other: &Self) {
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
        self.versions.is_empty() && self.cnmts.is_empty() && self.languages.is_empty()
    }
}

impl TitleVersionInfo {
    fn from_versions(title_id: String, mut versions: Vec<u64>) -> Self {
        versions.sort_unstable();
        versions.dedup();
        let latest_version = versions.iter().copied().max();
        Self { title_id, latest_version, versions }
    }

    fn merge_versions(&mut self, other: &Self) {
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

fn load_cache(path: &std::path::Path) -> Result<HashMap<String, TitleInfo>, TitleDbError> {
    let buf = std::fs::read_to_string(path)?;
    let raw: Vec<serde_json::Value> = serde_json::from_str(&buf)?;
    let mut map = HashMap::new();
    for v in raw {
        let obj = v.as_object().ok_or(TitleDbError::InvalidFormat)?;
        let id = obj.get("id").and_then(|v| v.as_str()).map(str::to_uppercase);
        let Some(id) = id else { continue };
        let icon_url = obj.get("icon_url").and_then(|v| v.as_str()).map(String::from);
        let banner_url = obj.get("banner_url").and_then(|v| v.as_str()).map(String::from);
        let name = obj.get("name").and_then(|v| v.as_str()).map(String::from);
        map.insert(id, TitleInfo { icon_url, banner_url, name });
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

fn load_artifact_cache(cache_dir: &std::path::Path) -> Result<TitleDbArtifacts, TitleDbError> {
    let versions = load_optional_artifact(cache_dir, "versions.json", parse_versions_json)?;
    let versions_txt = load_optional_artifact_txt(cache_dir, "versions.txt");
    let cnmts = load_optional_artifact(cache_dir, "cnmts.json", parse_cnmts_json)?;
    let languages = load_optional_artifact(cache_dir, "languages.json", parse_languages_json)?;

    let mut versions = versions.unwrap_or_default();
    for (id, info) in versions_txt {
        versions.entry(id).and_modify(|existing| existing.merge_versions(&info)).or_insert(info);
    }

    Ok(TitleDbArtifacts {
        versions,
        cnmts: cnmts.unwrap_or_default(),
        languages: languages.unwrap_or_default(),
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

fn save_artifact_cache(
    cache_dir: &std::path::Path,
    artifacts: &TitleDbArtifacts,
) -> Result<(), TitleDbError> {
    std::fs::create_dir_all(cache_dir)?;
    write_json_artifact(
        &cache_dir.join("versions.json"),
        artifacts.versions.values().map(|info| {
            serde_json::json!({
                "id": info.title_id,
                "latest_version": info.latest_version,
                "versions": info.versions,
            })
        }),
    )?;
    write_json_artifact(
        &cache_dir.join("cnmts.json"),
        artifacts.cnmts.values().map(|info| {
            serde_json::json!({
                "id": info.title_id,
                "type": info.title_type,
                "base_id": info.base_title_id,
                "version": info.version,
                "required_system_version": info.required_system_version,
            })
        }),
    )?;
    write_json_artifact(
        &cache_dir.join("languages.json"),
        artifacts.languages.values().map(|info| {
            serde_json::json!({
                "id": info.title_id,
                "languages": info.languages,
            })
        }),
    )?;
    Ok(())
}

fn write_json_artifact(
    path: &std::path::Path,
    values: impl IntoIterator<Item = serde_json::Value>,
) -> Result<(), TitleDbError> {
    let mut values: Vec<_> = values.into_iter().collect();
    values.sort_by(|a, b| {
        let a = a.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let b = b.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        a.cmp(b)
    });
    let buf = serde_json::to_string_pretty(&values)?;
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
        let update = cnmts.get("0100000000010800").expect("update cnmt");
        assert!(update.is_update_for("0100000000010000"));
        assert_eq!(update.version, Some(65_536));
        assert_eq!(update.required_system_version, Some(256));

        let dlc = cnmts.get("0100000000011001").expect("dlc cnmt");
        assert!(dlc.is_dlc_for("0100000000010000"));

        let languages = parse_languages_json(languages).expect("languages parse");
        assert_eq!(
            languages.get("0100000000010000").expect("language entry").languages,
            vec!["en".to_string(), "ja".to_string()]
        );
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
        let titledb = TitleDb::from_artifacts(artifacts);

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
}
