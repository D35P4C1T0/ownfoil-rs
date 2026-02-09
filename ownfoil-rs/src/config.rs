use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(
    name = "ownfoil-rs",
    version,
    about = "Minimal CyberFoil-compatible Tinfoil game server"
)]
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
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind: SocketAddr,
    pub library_root: PathBuf,
    pub auth_file: Option<PathBuf>,
    pub public_shop: bool,
    pub scan_interval_seconds: u64,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid config in {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("invalid boolean value for env var {key}: {value}")]
    InvalidEnvBool { key: String, value: String },
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    bind: Option<SocketAddr>,
    #[serde(alias = "library_folder")]
    library_root: Option<PathBuf>,
    auth_file: Option<PathBuf>,
    public_shop: Option<bool>,
    scan_interval_seconds: Option<u64>,
}

impl AppConfig {
    pub fn from_cli(cli: Cli) -> Result<Self, ConfigError> {
        let from_file = read_file_config(cli.config.as_deref())?;
        let env_public_shop = read_public_shop_env()?;

        let bind = cli
            .bind
            .or(from_file.bind)
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 8465)));
        let library_root = cli
            .library_root
            .or(from_file.library_root)
            .unwrap_or_else(|| PathBuf::from("./library"));
        let auth_file = cli.auth_file.or(from_file.auth_file);
        let public_shop = env_public_shop.or(from_file.public_shop).unwrap_or(false);
        let scan_interval_seconds = cli
            .scan_interval_seconds
            .or(from_file.scan_interval_seconds)
            .unwrap_or(30)
            .max(1);

        Ok(Self {
            bind,
            library_root,
            auth_file,
            public_shop,
            scan_interval_seconds,
        })
    }
}

fn read_file_config(path: Option<&Path>) -> Result<FileConfig, ConfigError> {
    let Some(path) = path else {
        return Ok(FileConfig::default());
    };

    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;

    toml::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })
}

fn read_public_shop_env() -> Result<Option<bool>, ConfigError> {
    if let Some(value) = read_env_bool("OWNFOIL_PUBLIC")? {
        return Ok(Some(value));
    }
    read_env_bool("OWNFOIL_SHOP_PUBLIC")
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

fn parse_bool_value(key: &str, raw: &str) -> Result<bool, ConfigError> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::InvalidEnvBool {
            key: String::from(key),
            value: String::from(raw),
        }),
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
