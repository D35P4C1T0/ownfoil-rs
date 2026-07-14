//! Configuration: CLI args, config file, and runtime merge.
//!
//! Priority: CLI flags > config file > defaults. `data_dir` defaults to `./data`
//! or `$XDG_DATA_HOME/ownfoil-rs` when set.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Deserialize;
use thiserror::Error;

use crate::settings::{Settings, SettingsError};
use crate::shop::{ShopConfig, validate_public_key_pem};

#[derive(Debug, Parser)]
#[command(name = "ownfoil-rs", version, about = "Minimal CyberFoil-compatible Tinfoil game server")]
pub struct Cli {
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<SocketAddr>,

    #[arg(
        long = "library-folder",
        short = 'l',
        visible_alias = "library-root",
        value_name = "DIR"
    )]
    pub library_root: Option<PathBuf>,

    #[arg(long, value_name = "FILE")]
    pub auth_file: Option<PathBuf>,

    #[arg(long, value_name = "SECONDS")]
    pub scan_interval_seconds: Option<u64>,

    #[arg(long, short = 'c', value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Ownfoil-compatible mutable YAML settings file.
    #[arg(long, value_name = "FILE")]
    pub settings: Option<PathBuf>,
}

/// Resolved application configuration after merging CLI, file, and env.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind: SocketAddr,
    pub library_root: PathBuf,
    pub library_roots: Vec<PathBuf>,
    pub auth_file: Option<PathBuf>,
    pub public_shop: bool,
    pub insecure_admin_cookie: bool,
    pub scan_interval_seconds: u64,
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub settings_path: PathBuf,
    pub db_path: PathBuf,
    pub keys_path: PathBuf,
    pub settings: Settings,
    pub titledb: TitleDbConfig,
    pub shop: ShopConfig,
}

/// `TitleDB` settings: region, language, refresh interval, optional URL override.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TitleDbConfig {
    pub enabled: bool,
    pub region: String,
    pub language: String,
    #[serde(default = "default_titledb_refresh")]
    pub refresh_interval: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_override: Option<String>,
}

fn default_titledb_refresh() -> String {
    "24h".to_string()
}

impl Default for TitleDbConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            region: "US".to_string(),
            language: "en".to_string(),
            refresh_interval: "24h".to_string(),
            url_override: Some(
                "https://nightly.link/a1ex4/ownfoil/workflows/region_titles/master/titledb.zip"
                    .to_string(),
            ),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read { path: String, source: std::io::Error },
    #[error("invalid config in {path}: {source}")]
    Parse { path: String, source: toml::de::Error },
    #[error("invalid boolean value for env var {key}: {value}")]
    InvalidEnvBool { key: String, value: String },
    #[error("library root {path} does not exist or is not a directory")]
    LibraryRootInvalid { path: String },
    #[error("auth file {path} does not exist")]
    AuthFileNotFound { path: String },
    #[error("invalid shop public key")]
    InvalidShopPublicKey,
    #[error(transparent)]
    OwnfoilSettings(#[from] SettingsError),
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    bind: Option<SocketAddr>,
    #[serde(alias = "library_folder")]
    library_root: Option<PathBuf>,
    auth_file: Option<PathBuf>,
    public_shop: Option<bool>,
    insecure_admin_cookie: Option<bool>,
    scan_interval_seconds: Option<u64>,
    titledb: Option<TitleDbConfig>,
    shop: Option<ShopConfig>,
}

impl AppConfig {
    pub fn from_cli(cli: Cli) -> Result<Self, ConfigError> {
        let config_path = cli.config.as_deref();
        let from_file = read_file_config(config_path)?;
        let from_runtime = read_runtime_config(config_path)?;
        let settings_path = resolve_settings_path(cli.settings);
        let settings_exists = settings_path.exists();
        let mut settings = Settings::load(&settings_path)?;
        let env_public_shop = read_public_shop_env()?;
        let env_insecure_admin_cookie = read_insecure_admin_cookie_env()?;

        let bind =
            cli.bind.or(from_file.bind).unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 8465)));
        let explicit_library = cli.library_root.or(from_file.library_root);
        if !settings_exists && explicit_library.is_none() && !Path::new("/games").is_dir() {
            settings.library.paths = vec![PathBuf::from("./library")];
        }
        let library_roots =
            explicit_library.map_or_else(|| settings.library.paths.clone(), |path| vec![path]);
        settings.library.paths.clone_from(&library_roots);
        let library_root =
            library_roots.first().cloned().unwrap_or_else(|| PathBuf::from("./library"));
        let auth_file = cli.auth_file.or(from_file.auth_file);
        let public_shop = env_public_shop.or(from_file.public_shop).unwrap_or(settings.shop.public);
        settings.shop.public = public_shop;
        let insecure_admin_cookie =
            env_insecure_admin_cookie.or(from_file.insecure_admin_cookie).unwrap_or(false);
        let scan_interval_seconds =
            cli.scan_interval_seconds.or(from_file.scan_interval_seconds).unwrap_or(30).max(1);

        let config_dir =
            settings_path.parent().map_or_else(|| PathBuf::from("./config"), Path::to_path_buf);
        let data_dir = if config_dir == Path::new("/app/config") {
            PathBuf::from("/app/data")
        } else {
            config_path
                .and_then(|p| p.parent())
                .map_or_else(|| PathBuf::from("./data"), |p| p.join("data"))
        };

        let mut titledb = from_runtime.titledb.or(from_file.titledb).unwrap_or_default();
        titledb.region.clone_from(&settings.titles.region);
        titledb.language.clone_from(&settings.titles.language);
        let mut shop = from_runtime.shop.or(from_file.shop).unwrap_or_else(|| ShopConfig {
            motd: settings.shop.motd.clone(),
            encrypt: settings.shop.clients.tinfoil.encrypt,
            ..ShopConfig::default()
        });
        if let Some(value) = read_shop_encrypt_env()? {
            shop.encrypt = value;
        }
        if let Some(value) = read_tinfoil_only_mode_env()? {
            shop.tinfoil_only_mode = value;
        }
        if let Some(value) = read_shop_motd_env()? {
            shop.motd = value;
        }
        if let Some(value) = read_shop_public_key_env()? {
            shop.public_key = value;
        }

        let config = Self {
            bind,
            library_root,
            library_roots,
            auth_file,
            public_shop,
            insecure_admin_cookie,
            scan_interval_seconds,
            db_path: data_dir.join("ownfoil.db"),
            data_dir,
            keys_path: config_dir.join("keys.txt"),
            config_dir,
            settings_path,
            settings,
            titledb,
            shop,
        };

        validate_config(&config)?;
        Ok(config)
    }
}

fn validate_config(config: &AppConfig) -> Result<(), ConfigError> {
    for library_root in &config.library_roots {
        if !library_root.exists() || !library_root.is_dir() {
            return Err(ConfigError::LibraryRootInvalid {
                path: library_root.display().to_string(),
            });
        }
    }
    if config.auth_file.as_ref().is_some_and(|path| !path.exists()) {
        let path =
            config.auth_file.as_ref().map_or_else(String::new, |path| path.display().to_string());
        return Err(ConfigError::AuthFileNotFound { path });
    }

    if !config.shop.public_key.trim().is_empty()
        && validate_public_key_pem(config.shop.public_key.trim()).is_err()
    {
        return Err(ConfigError::InvalidShopPublicKey);
    }

    Ok(())
}

fn resolve_settings_path(cli_path: Option<PathBuf>) -> PathBuf {
    cli_path.or_else(|| std::env::var_os("OWNFOIL_SETTINGS").map(PathBuf::from)).unwrap_or_else(
        || {
            if Path::new("/app/config").is_dir() {
                PathBuf::from("/app/config/settings.yaml")
            } else {
                PathBuf::from("./config/settings.yaml")
            }
        },
    )
}

fn read_file_config(path: Option<&Path>) -> Result<FileConfig, ConfigError> {
    let Some(path) = path else {
        return Ok(FileConfig::default());
    };

    let raw = std::fs::read_to_string(path)
        .map_err(|source| ConfigError::Read { path: path.display().to_string(), source })?;

    toml::from_str(&raw)
        .map_err(|source| ConfigError::Parse { path: path.display().to_string(), source })
}

#[derive(Debug, Default)]
struct RuntimeConfigState {
    titledb: Option<TitleDbConfig>,
    shop: Option<ShopConfig>,
}

#[derive(Debug, Deserialize)]
struct RuntimeConfig {
    titledb: Option<TitleDbConfig>,
    shop: Option<ShopConfig>,
}

fn read_runtime_config(config_path: Option<&Path>) -> Result<RuntimeConfigState, ConfigError> {
    let data_dir = config_path
        .and_then(|p| p.parent())
        .map_or_else(|| PathBuf::from("./data"), |p| p.join("data"));
    let runtime_path = data_dir.join("settings.toml");
    if !runtime_path.exists() {
        return Ok(RuntimeConfigState::default());
    }
    let raw = std::fs::read_to_string(&runtime_path)
        .map_err(|source| ConfigError::Read { path: runtime_path.display().to_string(), source })?;
    let parsed: RuntimeConfig = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: runtime_path.display().to_string(),
        source,
    })?;
    Ok(RuntimeConfigState { titledb: parsed.titledb, shop: parsed.shop })
}

fn read_public_shop_env() -> Result<Option<bool>, ConfigError> {
    if let Some(value) = read_env_bool("OWNFOIL_PUBLIC")? {
        return Ok(Some(value));
    }
    read_env_bool("OWNFOIL_SHOP_PUBLIC")
}

fn read_insecure_admin_cookie_env() -> Result<Option<bool>, ConfigError> {
    read_env_bool("OWNFOIL_INSECURE_ADMIN_COOKIE")
}

fn read_shop_encrypt_env() -> Result<Option<bool>, ConfigError> {
    if let Some(value) = read_env_bool("OWNFOIL_SHOP_ENCRYPT")? {
        return Ok(Some(value));
    }
    read_env_bool("AEROFOIL_SHOP_ENCRYPT")
}

fn read_tinfoil_only_mode_env() -> Result<Option<bool>, ConfigError> {
    if let Some(value) = read_env_bool("OWNFOIL_TINFOIL_ONLY_MODE")? {
        return Ok(Some(value));
    }
    read_env_bool("AEROFOIL_TINFOIL_ONLY_MODE")
}

fn read_shop_motd_env() -> Result<Option<String>, ConfigError> {
    if let Some(value) = read_env_string("OWNFOIL_SHOP_MOTD")? {
        return Ok(Some(value));
    }
    read_env_string("AEROFOIL_SHOP_MOTD")
}

fn read_shop_public_key_env() -> Result<Option<String>, ConfigError> {
    if let Some(value) = read_env_string("OWNFOIL_SHOP_PUBLIC_KEY")? {
        return Ok(Some(value));
    }
    read_env_string("AEROFOIL_SHOP_PUBLIC_KEY")
}

fn read_env_bool(key: &str) -> Result<Option<bool>, ConfigError> {
    match std::env::var(key) {
        Ok(value) => parse_bool_value(key, &value).map(Some),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError::InvalidEnvBool {
            key: String::from(key),
            value: String::from("<non-unicode>"),
        }),
    }
}

fn read_env_string(key: &str) -> Result<Option<String>, ConfigError> {
    match std::env::var(key) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError::InvalidEnvBool {
            key: String::from(key),
            value: String::from("<non-unicode>"),
        }),
    }
}

fn parse_bool_value(key: &str, raw: &str) -> Result<bool, ConfigError> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::InvalidEnvBool { key: String::from(key), value: String::from(raw) }),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_bool_value;

    #[test]
    fn parse_bool_value_accepts_common_true_values() {
        assert_eq!(parse_bool_value("K", "true").ok(), Some(true));
        assert_eq!(parse_bool_value("K", "1").ok(), Some(true));
        assert_eq!(parse_bool_value("K", "YES").ok(), Some(true));
        assert_eq!(parse_bool_value("K", " on ").ok(), Some(true));
    }

    #[test]
    fn parse_bool_value_accepts_common_false_values() {
        assert_eq!(parse_bool_value("K", "false").ok(), Some(false));
        assert_eq!(parse_bool_value("K", "0").ok(), Some(false));
        assert_eq!(parse_bool_value("K", "NO").ok(), Some(false));
        assert_eq!(parse_bool_value("K", " off ").ok(), Some(false));
    }

    #[test]
    fn parse_bool_value_rejects_invalid_values() {
        assert!(parse_bool_value("K", "maybe").is_err());
    }
}
