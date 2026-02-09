use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

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
            .map(|known_password| known_password == password)
            .unwrap_or(false)
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
            .users
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
}
