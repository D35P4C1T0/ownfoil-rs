//! Ownfoil-compatible YAML settings.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub library: LibrarySettings,
    pub titles: TitleSettings,
    pub shop: ShopSettings,
    pub scheduler: SchedulerSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LibrarySettings {
    pub paths: Vec<PathBuf>,
    pub management: LibraryManagementSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LibraryManagementSettings {
    pub compress_files: bool,
    pub delete_older_updates: bool,
    pub organizer: OrganizerSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrganizerSettings {
    pub enabled: bool,
    pub remove_empty_folders: bool,
    pub windows_compatible: bool,
    pub templates: OrganizerTemplates,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrganizerTemplates {
    pub base: String,
    pub update: String,
    pub dlc: String,
    pub multi: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TitleSettings {
    pub language: String,
    pub region: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShopSettings {
    pub host: String,
    pub public: bool,
    pub motd: String,
    pub clients: ClientSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientSettings {
    pub cyberfoil: CyberFoilSettings,
    pub tinfoil: TinfoilSettings,
    pub sphaira: SphairaSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CyberFoilSettings {
    pub enabled: bool,
    pub hauth: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[allow(non_snake_case)]
pub struct TinfoilSettings {
    pub enabled: bool,
    pub encrypt: bool,
    pub clientCertPub: String,
    pub clientCertKey: String,
    pub hauth: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SphairaSettings {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerSettings {
    pub scan_interval: String,
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("failed to read settings {path}: {source}")]
    Read { path: String, source: std::io::Error },
    #[error("failed to parse settings {path}: {source}")]
    Parse { path: String, source: serde_yaml::Error },
    #[error("failed to serialize settings: {0}")]
    Serialize(#[from] serde_yaml::Error),
    #[error("failed to write settings {path}: {source}")]
    Write { path: String, source: std::io::Error },
    #[error("invalid scheduler interval: {0}")]
    InvalidInterval(String),
    #[error("settings require at least one library path")]
    MissingLibraryPath,
}

#[allow(clippy::derivable_impls)]
impl Default for Settings {
    fn default() -> Self {
        Self {
            library: LibrarySettings::default(),
            titles: TitleSettings::default(),
            shop: ShopSettings::default(),
            scheduler: SchedulerSettings::default(),
        }
    }
}

impl Default for LibrarySettings {
    fn default() -> Self {
        Self {
            paths: vec![PathBuf::from("/games")],
            management: LibraryManagementSettings::default(),
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for LibraryManagementSettings {
    fn default() -> Self {
        Self {
            compress_files: false,
            delete_older_updates: false,
            organizer: OrganizerSettings::default(),
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for OrganizerSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            remove_empty_folders: false,
            windows_compatible: false,
            templates: OrganizerTemplates::default(),
        }
    }
}

impl Default for OrganizerTemplates {
    fn default() -> Self {
        Self {
            base: "{titleName}/{titleName} [{appId}][v{appVersion}]".to_string(),
            update: "{titleName}/{titleName} [{appId}][v{appVersion}]".to_string(),
            dlc: "{titleName}/{appName} [{appId}][v{appVersion}]".to_string(),
            multi: "{titleName}/{titleName} [{titleId}]".to_string(),
        }
    }
}

impl Default for TitleSettings {
    fn default() -> Self {
        Self { language: "en".to_string(), region: "US".to_string() }
    }
}

impl Default for ShopSettings {
    fn default() -> Self {
        Self {
            host: String::new(),
            public: false,
            motd: "Welcome to your own shop!".to_string(),
            clients: ClientSettings::default(),
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            cyberfoil: CyberFoilSettings::default(),
            tinfoil: TinfoilSettings::default(),
            sphaira: SphairaSettings::default(),
        }
    }
}

impl Default for CyberFoilSettings {
    fn default() -> Self {
        Self { enabled: true, hauth: BTreeMap::new() }
    }
}

impl Default for TinfoilSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            encrypt: true,
            clientCertPub: "-----BEGIN PUBLIC KEY-----".to_string(),
            clientCertKey: "-----BEGIN PRIVATE KEY-----".to_string(),
            hauth: BTreeMap::new(),
        }
    }
}

impl Default for SphairaSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for SchedulerSettings {
    fn default() -> Self {
        Self { scan_interval: "12h".to_string() }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .map_err(|source| SettingsError::Read { path: path.display().to_string(), source })?;
        let mut value: Value = serde_yaml::from_str(&raw)
            .map_err(|source| SettingsError::Parse { path: path.display().to_string(), source })?;
        migrate_legacy_shop(&mut value);
        let settings: Self = serde_yaml::from_value(value)
            .map_err(|source| SettingsError::Parse { path: path.display().to_string(), source })?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| SettingsError::Write {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let serialized = serde_yaml::to_string(self)?;
        let temp_path = path.with_extension("yaml.tmp");
        fs::write(&temp_path, serialized).map_err(|source| SettingsError::Write {
            path: temp_path.display().to_string(),
            source,
        })?;
        fs::rename(&temp_path, path)
            .map_err(|source| SettingsError::Write { path: path.display().to_string(), source })
    }

    pub fn validate(&self) -> Result<(), SettingsError> {
        if self.library.paths.is_empty() {
            return Err(SettingsError::MissingLibraryPath);
        }
        validate_interval(&self.scheduler.scan_interval)
    }

    pub fn redacted(&self) -> Self {
        let mut settings = self.clone();
        settings.shop.clients.tinfoil.hauth.clear();
        settings.shop.clients.cyberfoil.hauth.clear();
        settings.shop.clients.tinfoil.clientCertKey.clear();
        settings
    }
}

pub fn validate_interval(raw: &str) -> Result<(), SettingsError> {
    if raw == "0" {
        return Ok(());
    }
    let Some((number, unit)) = raw.split_at_checked(raw.len().saturating_sub(1)) else {
        return Err(SettingsError::InvalidInterval(raw.to_string()));
    };
    if number.is_empty()
        || number.parse::<u64>().ok().map_or(true, |value| value == 0)
        || !matches!(unit, "s" | "m" | "h" | "d")
    {
        return Err(SettingsError::InvalidInterval(raw.to_string()));
    }
    Ok(())
}

fn migrate_legacy_shop(root: &mut Value) {
    let Some(shop) = mapping_at_mut(root, "shop") else {
        return;
    };
    let legacy_keys = ["encrypt", "hauth", "clientCertKey", "clientCertPub"];
    let mut legacy = Mapping::new();
    for key in legacy_keys {
        let yaml_key = Value::String(key.to_string());
        if let Some(value) = shop.remove(&yaml_key) {
            legacy.insert(yaml_key, value);
        }
    }
    if legacy.is_empty() {
        return;
    }
    let clients = mapping_entry(shop, "clients");
    let tinfoil = mapping_entry(clients, "tinfoil");
    for (key, value) in legacy {
        tinfoil.entry(key).or_insert(value);
    }
}

fn mapping_at_mut<'a>(value: &'a mut Value, key: &str) -> Option<&'a mut Mapping> {
    value.as_mapping_mut()?.get_mut(Value::String(key.to_string()))?.as_mapping_mut()
}

fn mapping_entry<'a>(mapping: &'a mut Mapping, key: &str) -> &'a mut Mapping {
    mapping
        .entry(Value::String(key.to_string()))
        .or_insert_with(|| Value::Mapping(Mapping::new()))
        .as_mapping_mut()
        .unwrap_or_else(|| panic!("settings migration expected mapping for {key}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{Settings, validate_interval};

    #[test]
    fn defaults_match_upstream() {
        let settings = Settings::default();
        assert_eq!(settings.library.paths, [std::path::PathBuf::from("/games")]);
        assert_eq!(settings.titles.region, "US");
        assert_eq!(settings.scheduler.scan_interval, "12h");
        assert!(settings.shop.clients.tinfoil.encrypt);
        assert!(settings.shop.clients.sphaira.enabled);
    }

    #[test]
    fn interval_validation_matches_upstream_format() {
        for valid in ["0", "1s", "30m", "12h", "2d"] {
            assert!(validate_interval(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "0s", "2", "1w", "-1h", "1.5h"] {
            assert!(validate_interval(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn loads_partial_yaml_and_migrates_legacy_shop_keys() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("settings.yaml");
        std::fs::write(
            &path,
            "library:\n  paths: [/library]\nshop:\n  encrypt: false\n  hauth:\n    example.org: abc\n",
        )?;

        let settings = Settings::load(&path)?;
        assert_eq!(settings.library.paths, [std::path::PathBuf::from("/library")]);
        assert!(!settings.shop.clients.tinfoil.encrypt);
        assert_eq!(
            settings.shop.clients.tinfoil.hauth,
            BTreeMap::from([("example.org".to_string(), "abc".to_string())])
        );
        assert!(settings.shop.clients.cyberfoil.enabled);
        Ok(())
    }

    #[test]
    fn save_is_round_trip_and_redaction_removes_secrets() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config/settings.yaml");
        let mut settings = Settings::default();
        settings
            .shop
            .clients
            .tinfoil
            .hauth
            .insert("shop.example".to_string(), "secret".to_string());
        settings.shop.clients.tinfoil.clientCertKey = "private".to_string();
        settings.save(&path)?;

        assert_eq!(Settings::load(&path)?, settings);
        let redacted = settings.redacted();
        assert!(redacted.shop.clients.tinfoil.hauth.is_empty());
        assert!(redacted.shop.clients.tinfoil.clientCertKey.is_empty());
        Ok(())
    }
}
