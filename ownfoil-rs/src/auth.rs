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

use std::path::Path;

use dashmap::DashMap;
use scrypt::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use scrypt::{Scrypt, password_hash};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use thiserror::Error;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct AuthSettings {
    users: DashMap<String, AuthRecord>,
}

#[derive(Debug, Clone)]
struct AuthRecord {
    credential: Credential,
    admin_access: bool,
    shop_access: bool,
    backup_access: bool,
}

#[derive(Debug, Clone)]
enum Credential {
    Plaintext(String),
    Scrypt(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthRoles {
    pub admin_access: bool,
    pub shop_access: bool,
    pub backup_access: bool,
}

impl AuthSettings {
    pub fn from_users(users: Vec<AuthUser>) -> Self {
        let mapped = DashMap::new();

        for user in users {
            let username = user.username.trim().to_string();
            let password = user.password.trim().to_string();
            if username.is_empty() || password.is_empty() {
                continue;
            }
            mapped.insert(
                username,
                AuthRecord {
                    credential: Credential::Plaintext(password),
                    admin_access: true,
                    shop_access: true,
                    backup_access: true,
                },
            );
        }

        Self { users: mapped }
    }

    pub fn is_enabled(&self) -> bool {
        !self.users.is_empty()
    }

    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    pub fn usernames(&self) -> Vec<String> {
        let mut users = self.users.iter().map(|entry| entry.key().clone()).collect::<Vec<_>>();
        users.sort();
        users
    }

    pub fn is_authorized(&self, username: &str, password: &str) -> bool {
        self.users.get(username).is_some_and(|record| match &record.credential {
            Credential::Plaintext(known_password) => {
                password.as_bytes().ct_eq(known_password.as_bytes()).into()
            }
            Credential::Scrypt(hash) => {
                PasswordHash::new(hash).ok().is_some_and(|parsed| {
                    Scrypt.verify_password(password.as_bytes(), &parsed).is_ok()
                }) || verify_werkzeug_scrypt(password, hash)
            }
        })
    }

    pub fn roles(&self, username: &str) -> Option<AuthRoles> {
        self.users.get(username).map(|record| AuthRoles {
            admin_access: record.admin_access,
            shop_access: record.shop_access,
            backup_access: record.backup_access,
        })
    }

    pub fn upsert_hashed_user(&self, username: String, password_hash: String, roles: AuthRoles) {
        self.users.insert(
            username,
            AuthRecord {
                credential: Credential::Scrypt(password_hash),
                admin_access: roles.admin_access,
                shop_access: roles.shop_access,
                backup_access: roles.backup_access,
            },
        );
    }

    pub fn remove_user(&self, username: &str) {
        self.users.remove(username);
    }
}

pub fn hash_password(password: &str) -> Result<String, password_hash::Error> {
    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    Scrypt.hash_password(password.as_bytes(), &salt).map(|hash| hash.to_string())
}

fn verify_werkzeug_scrypt(password: &str, encoded: &str) -> bool {
    let mut sections = encoded.split('$');
    let Some(method) = sections.next() else {
        return false;
    };
    let Some(salt) = sections.next() else {
        return false;
    };
    let Some(expected) = sections.next() else {
        return false;
    };
    if sections.next().is_some() || expected.len() % 2 != 0 {
        return false;
    }
    let mut parameters = method.split(':');
    if parameters.next() != Some("scrypt") {
        return false;
    }
    let Some(n) = parameters.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    let Some(r) = parameters.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    let Some(p) = parameters.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    if parameters.next().is_some() || !n.is_power_of_two() {
        return false;
    }
    let log_n = u8::try_from(n.ilog2()).unwrap_or(u8::MAX);
    let Ok(params) = scrypt::Params::new(log_n, r, p, expected.len() / 2) else {
        return false;
    };
    let mut derived = vec![0_u8; expected.len() / 2];
    if scrypt::scrypt(password.as_bytes(), salt.as_bytes(), &params, &mut derived).is_err() {
        return false;
    }
    let actual = hex::encode(derived);
    actual.as_bytes().ct_eq(expected.as_bytes()).into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthUser {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Error)]
pub enum AuthFileError {
    #[error("failed to read auth file {path}: {source}")]
    Read { path: String, source: std::io::Error },
    #[error("invalid auth config in {path}: {source}")]
    Parse { path: String, source: toml::de::Error },
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

    let raw = std::fs::read_to_string(path)
        .map_err(|source| AuthFileError::Read { path: path.display().to_string(), source })?;

    let parsed: AuthFile = toml::from_str(&raw)
        .map_err(|source| AuthFileError::Parse { path: path.display().to_string(), source })?;

    let mut users = Vec::new();

    if let (Some(username), Some(password)) = (parsed.username, parsed.password) {
        users.push(AuthUser { username, password });
    }

    if let Some(more) = parsed.users {
        users.extend(
            more.into_iter()
                .map(|entry| AuthUser { username: entry.username, password: entry.password }),
        );
    }

    let settings = AuthSettings::from_users(users.clone());
    if settings.is_enabled() {
        let normalized = settings.usernames();
        let normalized_users =
            normalized
                .into_iter()
                .filter_map(|username| {
                    users.iter().rev().find(|user| user.username.trim() == username).map(|user| {
                        AuthUser { username, password: user.password.trim().to_string() }
                    })
                })
                .collect::<Vec<_>>();
        Ok(normalized_users)
    } else {
        Err(AuthFileError::EmptyCredentials { path: path.display().to_string() })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use anyhow::Result;
    use tempfile::tempdir;

    use super::{AuthRoles, AuthSettings, AuthUser, hash_password, load_users_from_file};

    #[test]
    fn auth_settings_merges_duplicate_users() {
        let settings = AuthSettings::from_users(vec![
            AuthUser { username: String::from("alice"), password: String::from("pw1") },
            AuthUser { username: String::from("alice"), password: String::from("pw2") },
            AuthUser { username: String::from("bob"), password: String::from("pw3") },
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

    #[test]
    fn scrypt_users_verify_without_storing_plaintext() -> Result<()> {
        let settings = AuthSettings::from_users(Vec::new());
        let hash = hash_password("correct horse")?;
        settings.upsert_hashed_user(
            "admin".to_string(),
            hash,
            AuthRoles { admin_access: true, shop_access: true, backup_access: true },
        );

        assert!(settings.is_authorized("admin", "correct horse"));
        assert!(!settings.is_authorized("admin", "wrong"));
        assert!(settings.roles("admin").is_some_and(|roles| roles.admin_access));
        Ok(())
    }
}
