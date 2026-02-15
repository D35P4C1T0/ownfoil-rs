//! HTTP Basic auth credentials loaded from a TOML file.
//!
//! ## Auth file format
//!
//! The auth file supports two styles:
//!
//! **Single user (flat):**
//! ```toml
//! username = "admin"
//! password = "secret"
//! ```
//!
//! **Multiple users (array):**
//! ```toml
//! [[users]]
//! username = "alice"
//! password = "pw1"
//!
//! [[users]]
//! username = "bob"
//! password = "pw2"
//! ```
//!
//! Both can be combined; the single `username`/`password` pair is merged with `[[users]]`.
//! Duplicate usernames are deduplicated (last wins). Empty usernames or passwords are skipped.
//!
//! **Security:** Use `chmod 600` on the auth file. The server warns if it is world-readable (Unix).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use subtle::ConstantTimeEq;
use thiserror::Error;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct AuthSettings {
    users: BTreeMap<String, String>,
}

impl AuthSettings {
    pub fn from_users(users: Vec<AuthUser>) -> Self {
        let mut mapped = BTreeMap::new();

        for user in users {
            let username = user.username.trim().to_string();
            let password = user.password.trim().to_string();
            if username.is_empty() || password.is_empty() {
                continue;
            }
            mapped.insert(username, password);
        }

        Self { users: mapped }
    }

    pub fn is_enabled(&self) -> bool {
        !self.users.is_empty()
    }

    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    pub fn is_authorized(&self, username: &str, password: &str) -> bool {
        self.users
            .get(username)
            .is_some_and(|known_password| {
                let a = password.as_bytes();
                let b = known_password.as_bytes();
                a.ct_eq(b).into()
            })
    }

    fn into_users(self) -> BTreeMap<String, String> {
        self.users
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthUser {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Error)]
pub enum AuthFileError {
    #[error("failed to read auth file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid auth config in {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("auth file {path} does not define valid credentials")]
    EmptyCredentials { path: String },
}

#[derive(Debug, Default, Deserialize)]
struct AuthFile {
    username: Option<String>,
    password: Option<String>,
    users: Option<Vec<AuthUserEntry>>,
}

#[derive(Debug, Clone, Deserialize)]
struct AuthUserEntry {
    username: String,
    password: String,
}

/// Load auth settings from a file. Returns empty settings if path is None.
/// Warns if the auth file is world-readable (Unix only).
pub fn load_auth(path: Option<&Path>) -> Result<AuthSettings, AuthFileError> {
    if let Some(p) = path {
        check_auth_file_permissions(p);
    }
    let users = load_users_from_file(path)?;
    Ok(AuthSettings::from_users(users))
}

/// Warn if auth file is world-readable. No-op on non-Unix.
#[cfg(unix)]
fn check_auth_file_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o004 != 0 {
            warn!(
                path = %path.display(),
                "auth file is world-readable; consider chmod 600"
            );
        }
    }
}

#[cfg(not(unix))]
fn check_auth_file_permissions(_path: &Path) {}

/// Load users from auth file. Requires at least one valid credential when path is Some.
pub fn load_users_from_file(path: Option<&Path>) -> Result<Vec<AuthUser>, AuthFileError> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };

    let raw = std::fs::read_to_string(path).map_err(|source| AuthFileError::Read {
        path: path.display().to_string(),
        source,
    })?;

    let parsed: AuthFile = toml::from_str(&raw).map_err(|source| AuthFileError::Parse {
        path: path.display().to_string(),
        source,
    })?;

    let mut users = Vec::new();

    if let (Some(username), Some(password)) = (parsed.username, parsed.password) {
        users.push(AuthUser { username, password });
    }

    if let Some(more) = parsed.users {
        users.extend(more.into_iter().map(|entry| AuthUser {
            username: entry.username,
            password: entry.password,
        }));
    }

    let settings = AuthSettings::from_users(users);
    if settings.is_enabled() {
        Ok(settings
            .into_users()
            .into_iter()
            .map(|(username, password)| AuthUser { username, password })
            .collect::<Vec<_>>())
    } else {
        Err(AuthFileError::EmptyCredentials {
            path: path.display().to_string(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use anyhow::Result;
    use tempfile::tempdir;

    use super::{load_users_from_file, AuthSettings, AuthUser};

    #[test]
    fn auth_settings_merges_duplicate_users() {
        let settings = AuthSettings::from_users(vec![
            AuthUser {
                username: String::from("alice"),
                password: String::from("pw1"),
            },
            AuthUser {
                username: String::from("alice"),
                password: String::from("pw2"),
            },
            AuthUser {
                username: String::from("bob"),
                password: String::from("pw3"),
            },
        ]);

        assert!(settings.is_enabled());
        assert_eq!(settings.user_count(), 2);
        assert!(settings.is_authorized("alice", "pw2"));
        assert!(settings.is_authorized("bob", "pw3"));
    }

    #[test]
    fn auth_file_parses_single_and_list_users() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("auth.toml");
        std::fs::write(
            &path,
            "username = \"alice\"\npassword = \"pw1\"\n[[users]]\nusername = \"bob\"\npassword = \"pw2\"\n",
        )?;

        let users = load_users_from_file(Some(&path))?;

        assert_eq!(users.len(), 2);
        assert_eq!(users[0].username, "alice");
        assert_eq!(users[0].password, "pw1");
        assert_eq!(users[1].username, "bob");
        assert_eq!(users[1].password, "pw2");
        Ok(())
    }

    #[test]
    fn load_users_rejects_empty_credentials_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        std::fs::write(&path, "").unwrap();

        let result = load_users_from_file(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn load_users_rejects_file_with_only_empty_users() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.toml");
        std::fs::write(&path, "username = \"\"\npassword = \"\"\n").unwrap();

        let result = load_users_from_file(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn auth_rejects_wrong_password() {
        let settings = AuthSettings::from_users(vec![AuthUser {
            username: String::from("user"),
            password: String::from("correct"),
        }]);
        assert!(!settings.is_authorized("user", "wrong"));
    }

    #[test]
    fn auth_rejects_unknown_user() {
        let settings = AuthSettings::from_users(vec![AuthUser {
            username: String::from("alice"),
            password: String::from("secret"),
        }]);
        assert!(!settings.is_authorized("bob", "secret"));
    }
}
