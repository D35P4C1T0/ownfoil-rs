//! Persistent SQLite storage for Ownfoil concepts.
//!
//! This module is intentionally not wired into HTTP state yet. It defines the
//! durable schema and a small async-friendly API that route handlers can adopt
//! incrementally.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Storage {
    db_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    pub id: i64,
    pub path: String,
    pub last_scan: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLibrary {
    pub path: String,
    pub last_scan: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFile {
    pub id: i64,
    pub library_id: i64,
    pub path: String,
    pub folder: String,
    pub name: String,
    pub ext: String,
    pub size: i64,
    pub compressed: bool,
    pub multicontent: bool,
    pub download_count: i64,
    pub identification_status: IdentificationStatus,
    pub title_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewStoredFile {
    pub library_id: i64,
    pub path: String,
    pub folder: String,
    pub name: String,
    pub ext: String,
    pub size: i64,
    pub compressed: bool,
    pub multicontent: bool,
    pub download_count: i64,
    pub identification_status: IdentificationStatus,
    pub title_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentificationStatus {
    Unknown,
    Identified,
    Failed,
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Title {
    pub title_id: String,
    pub have_base: bool,
    pub up_to_date: bool,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: i64,
    pub title_id: String,
    pub file_id: Option<i64>,
    pub app_id: String,
    pub app_version: i64,
    pub app_type: AppType,
    pub owned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppType {
    Base,
    Update,
    Dlc,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub can_admin: bool,
    pub can_upload: bool,
    pub can_download: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUser {
    pub username: String,
    pub password_hash: String,
    pub can_admin: bool,
    pub can_upload: bool,
    pub can_download: bool,
    pub enabled: bool,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("storage task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub type Result<T> = std::result::Result<T, StorageError>;

impl Storage {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db_path = path.as_ref().to_path_buf();
        let init_path = db_path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_connection(&init_path)?;
            initialize_schema(&conn)
        })
        .await??;
        Ok(Self { db_path })
    }

    pub fn path(&self) -> &Path {
        &self.db_path
    }

    pub async fn upsert_library(&self, library: NewLibrary) -> Result<Library> {
        self.with_connection(move |conn| {
            conn.execute(
                r#"
                INSERT INTO libraries (path, last_scan)
                VALUES (?1, ?2)
                ON CONFLICT(path) DO UPDATE SET
                    last_scan = excluded.last_scan
                "#,
                params![library.path, library.last_scan],
            )?;
            library_by_path(conn, &library.path)
        })
        .await
    }

    pub async fn get_library(&self, id: i64) -> Result<Option<Library>> {
        self.with_connection(move |conn| {
            conn.query_row(
                "SELECT id, path, last_scan FROM libraries WHERE id = ?1",
                params![id],
                read_library,
            )
            .optional()
            .map_err(StorageError::from)
        })
        .await
    }

    pub async fn get_library_by_path(&self, path: impl Into<String>) -> Result<Option<Library>> {
        let path = path.into();
        self.with_connection(move |conn| {
            conn.query_row(
                "SELECT id, path, last_scan FROM libraries WHERE path = ?1",
                params![path],
                read_library,
            )
            .optional()
            .map_err(StorageError::from)
        })
        .await
    }

    pub async fn list_libraries(&self) -> Result<Vec<Library>> {
        self.with_connection(move |conn| {
            let mut stmt =
                conn.prepare("SELECT id, path, last_scan FROM libraries ORDER BY path")?;
            let rows = stmt.query_map([], read_library)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(StorageError::from)
        })
        .await
    }

    pub async fn delete_library(&self, id: i64) -> Result<bool> {
        self.with_connection(move |conn| {
            conn.execute("DELETE FROM libraries WHERE id = ?1", params![id])
                .map(|count| count > 0)
                .map_err(StorageError::from)
        })
        .await
    }

    pub async fn upsert_file(&self, file: NewStoredFile) -> Result<StoredFile> {
        self.with_connection(move |conn| {
            conn.execute(
                r#"
                INSERT INTO files (
                    library_id,
                    path,
                    folder,
                    name,
                    ext,
                    size,
                    compressed,
                    multicontent,
                    download_count,
                    identification_status,
                    title_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ON CONFLICT(library_id, path) DO UPDATE SET
                    folder = excluded.folder,
                    name = excluded.name,
                    ext = excluded.ext,
                    size = excluded.size,
                    compressed = excluded.compressed,
                    multicontent = excluded.multicontent,
                    download_count = excluded.download_count,
                    identification_status = excluded.identification_status,
                    title_id = excluded.title_id
                "#,
                params![
                    file.library_id,
                    file.path,
                    file.folder,
                    file.name,
                    file.ext,
                    file.size,
                    file.compressed,
                    file.multicontent,
                    file.download_count,
                    file.identification_status.as_str(),
                    file.title_id
                ],
            )?;
            file_by_library_path(conn, file.library_id, &file.path)
        })
        .await
    }

    pub async fn get_file(&self, id: i64) -> Result<Option<StoredFile>> {
        self.with_connection(move |conn| {
            conn.query_row(file_select("WHERE id = ?1").as_str(), params![id], read_file)
                .optional()
                .map_err(StorageError::from)
        })
        .await
    }

    pub async fn get_file_by_path(
        &self,
        library_id: i64,
        path: impl Into<String>,
    ) -> Result<Option<StoredFile>> {
        let path = path.into();
        self.with_connection(move |conn| {
            conn.query_row(
                file_select("WHERE library_id = ?1 AND path = ?2").as_str(),
                params![library_id, path],
                read_file,
            )
            .optional()
            .map_err(StorageError::from)
        })
        .await
    }

    pub async fn list_files_for_library(&self, library_id: i64) -> Result<Vec<StoredFile>> {
        self.with_connection(move |conn| {
            let sql = file_select("WHERE library_id = ?1 ORDER BY folder, name");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![library_id], read_file)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(StorageError::from)
        })
        .await
    }

    pub async fn delete_file(&self, id: i64) -> Result<bool> {
        self.with_connection(move |conn| {
            conn.execute("DELETE FROM files WHERE id = ?1", params![id])
                .map(|count| count > 0)
                .map_err(StorageError::from)
        })
        .await
    }

    pub async fn increment_file_download_count(&self, id: i64) -> Result<Option<StoredFile>> {
        self.with_connection(move |conn| {
            conn.execute(
                "UPDATE files SET download_count = download_count + 1 WHERE id = ?1",
                params![id],
            )?;
            conn.query_row(file_select("WHERE id = ?1").as_str(), params![id], read_file)
                .optional()
                .map_err(StorageError::from)
        })
        .await
    }

    pub async fn upsert_user(&self, user: NewUser) -> Result<User> {
        self.with_connection(move |conn| {
            conn.execute(
                r#"
                INSERT INTO users (
                    username,
                    password_hash,
                    can_admin,
                    can_upload,
                    can_download,
                    enabled
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(username) DO UPDATE SET
                    password_hash = excluded.password_hash,
                    can_admin = excluded.can_admin,
                    can_upload = excluded.can_upload,
                    can_download = excluded.can_download,
                    enabled = excluded.enabled
                "#,
                params![
                    user.username,
                    user.password_hash,
                    user.can_admin,
                    user.can_upload,
                    user.can_download,
                    user.enabled
                ],
            )?;
            user_by_username(conn, &user.username)
        })
        .await
    }

    pub async fn get_user(&self, id: i64) -> Result<Option<User>> {
        self.with_connection(move |conn| {
            conn.query_row(user_select("WHERE id = ?1").as_str(), params![id], read_user)
                .optional()
                .map_err(StorageError::from)
        })
        .await
    }

    pub async fn get_user_by_username(&self, username: impl Into<String>) -> Result<Option<User>> {
        let username = username.into();
        self.with_connection(move |conn| {
            conn.query_row(
                user_select("WHERE username = ?1").as_str(),
                params![username],
                read_user,
            )
            .optional()
            .map_err(StorageError::from)
        })
        .await
    }

    pub async fn list_users(&self) -> Result<Vec<User>> {
        self.with_connection(move |conn| {
            let mut stmt = conn.prepare(&user_select("ORDER BY username"))?;
            let rows = stmt.query_map([], read_user)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(StorageError::from)
        })
        .await
    }

    pub async fn delete_user(&self, id: i64) -> Result<bool> {
        self.with_connection(move |conn| {
            conn.execute("DELETE FROM users WHERE id = ?1", params![id])
                .map(|count| count > 0)
                .map_err(StorageError::from)
        })
        .await
    }

    async fn with_connection<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_connection(&db_path)?;
            f(&conn)
        })
        .await?
    }
}

impl IdentificationStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Identified => "identified",
            Self::Failed => "failed",
            Self::Ignored => "ignored",
        }
    }

    fn from_str(raw: &str) -> Self {
        match raw {
            "identified" => Self::Identified,
            "failed" => Self::Failed,
            "ignored" => Self::Ignored,
            _ => Self::Unknown,
        }
    }
}

impl AppType {
    #[allow(dead_code)]
    fn as_str(&self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Update => "update",
            Self::Dlc => "dlc",
            Self::Unknown => "unknown",
        }
    }

    #[allow(dead_code)]
    fn from_str(raw: &str) -> Self {
        match raw {
            "base" => Self::Base,
            "update" => Self::Update,
            "dlc" => Self::Dlc,
            _ => Self::Unknown,
        }
    }
}

fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(conn)
}

fn initialize_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS libraries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL UNIQUE,
            last_scan INTEGER
        );

        CREATE TABLE IF NOT EXISTS titles (
            title_id TEXT PRIMARY KEY,
            have_base INTEGER NOT NULL DEFAULT 0,
            up_to_date INTEGER NOT NULL DEFAULT 0,
            complete INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            library_id INTEGER NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
            path TEXT NOT NULL,
            folder TEXT NOT NULL,
            name TEXT NOT NULL,
            ext TEXT NOT NULL,
            size INTEGER NOT NULL,
            compressed INTEGER NOT NULL DEFAULT 0,
            multicontent INTEGER NOT NULL DEFAULT 0,
            download_count INTEGER NOT NULL DEFAULT 0,
            identification_status TEXT NOT NULL DEFAULT 'unknown',
            title_id TEXT REFERENCES titles(title_id) ON DELETE SET NULL,
            UNIQUE(library_id, path)
        );

        CREATE TABLE IF NOT EXISTS apps (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title_id TEXT NOT NULL REFERENCES titles(title_id) ON DELETE CASCADE,
            file_id INTEGER REFERENCES files(id) ON DELETE SET NULL,
            app_id TEXT NOT NULL,
            app_version INTEGER NOT NULL,
            app_type TEXT NOT NULL,
            owned INTEGER NOT NULL DEFAULT 0,
            UNIQUE(app_id, app_version, app_type, file_id)
        );

        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            can_admin INTEGER NOT NULL DEFAULT 0,
            can_upload INTEGER NOT NULL DEFAULT 0,
            can_download INTEGER NOT NULL DEFAULT 1,
            enabled INTEGER NOT NULL DEFAULT 1
        );
        "#,
    )
}

fn library_by_path(conn: &Connection, path: &str) -> Result<Library> {
    conn.query_row(
        "SELECT id, path, last_scan FROM libraries WHERE path = ?1",
        params![path],
        read_library,
    )
    .map_err(StorageError::from)
}

fn file_by_library_path(conn: &Connection, library_id: i64, path: &str) -> Result<StoredFile> {
    conn.query_row(
        file_select("WHERE library_id = ?1 AND path = ?2").as_str(),
        params![library_id, path],
        read_file,
    )
    .map_err(StorageError::from)
}

fn user_by_username(conn: &Connection, username: &str) -> Result<User> {
    conn.query_row(user_select("WHERE username = ?1").as_str(), params![username], read_user)
        .map_err(StorageError::from)
}

fn read_library(row: &rusqlite::Row<'_>) -> rusqlite::Result<Library> {
    Ok(Library { id: row.get(0)?, path: row.get(1)?, last_scan: row.get(2)? })
}

fn read_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredFile> {
    let status: String = row.get(10)?;
    Ok(StoredFile {
        id: row.get(0)?,
        library_id: row.get(1)?,
        path: row.get(2)?,
        folder: row.get(3)?,
        name: row.get(4)?,
        ext: row.get(5)?,
        size: row.get(6)?,
        compressed: row.get(7)?,
        multicontent: row.get(8)?,
        download_count: row.get(9)?,
        identification_status: IdentificationStatus::from_str(&status),
        title_id: row.get(11)?,
    })
}

fn read_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        username: row.get(1)?,
        password_hash: row.get(2)?,
        can_admin: row.get(3)?,
        can_upload: row.get(4)?,
        can_download: row.get(5)?,
        enabled: row.get(6)?,
    })
}

fn file_select(clause: &str) -> String {
    format!(
        r#"
        SELECT
            id,
            library_id,
            path,
            folder,
            name,
            ext,
            size,
            compressed,
            multicontent,
            download_count,
            identification_status,
            title_id
        FROM files
        {clause}
        "#
    )
}

fn user_select(clause: &str) -> String {
    format!(
        r#"
        SELECT
            id,
            username,
            password_hash,
            can_admin,
            can_upload,
            can_download,
            enabled
        FROM users
        {clause}
        "#
    )
}

#[cfg(test)]
mod tests {
    use super::{
        IdentificationStatus, NewLibrary, NewStoredFile, NewUser, Result, Storage, StoredFile,
    };

    async fn temp_storage() -> Result<(tempfile::TempDir, Storage)> {
        let dir = tempfile::tempdir()
            .map_err(|source| rusqlite::Error::ToSqlConversionFailure(Box::new(source)))?;
        let storage = Storage::open(dir.path().join("ownfoil.sqlite")).await?;
        Ok((dir, storage))
    }

    #[tokio::test]
    async fn initializes_schema_and_reopens_existing_database() -> Result<()> {
        let (_dir, storage) = temp_storage().await?;
        let db_path = storage.path().to_path_buf();
        let library = storage
            .upsert_library(NewLibrary {
                path: "/games".to_string(),
                last_scan: Some(1_735_689_600),
            })
            .await?;

        let reopened = Storage::open(db_path).await?;
        let libraries = reopened.list_libraries().await?;

        assert_eq!(libraries, vec![library]);
        Ok(())
    }

    #[tokio::test]
    async fn upserts_libraries_and_files() -> Result<()> {
        let (_dir, storage) = temp_storage().await?;
        let library = storage
            .upsert_library(NewLibrary { path: "/mnt/library".to_string(), last_scan: None })
            .await?;
        let updated_library = storage
            .upsert_library(NewLibrary { path: "/mnt/library".to_string(), last_scan: Some(42) })
            .await?;

        assert_eq!(library.id, updated_library.id);
        assert_eq!(updated_library.last_scan, Some(42));

        let file = storage.upsert_file(new_file(library.id, 100)).await?;
        let updated_file = storage.upsert_file(new_file(library.id, 200)).await?;
        let files = storage.list_files_for_library(library.id).await?;

        assert_eq!(file.id, updated_file.id);
        assert_eq!(files, vec![updated_file.clone()]);
        assert_eq!(updated_file.size, 200);
        assert_eq!(updated_file.identification_status, IdentificationStatus::Identified);
        Ok(())
    }

    #[tokio::test]
    async fn increments_download_count_and_deletes_file() -> Result<()> {
        let (_dir, storage) = temp_storage().await?;
        let library = storage
            .upsert_library(NewLibrary { path: "/mnt/library".to_string(), last_scan: None })
            .await?;
        let file = storage.upsert_file(new_file(library.id, 100)).await?;

        let downloaded = storage.increment_file_download_count(file.id).await?;
        assert_eq!(downloaded.map(|file| file.download_count), Some(1));

        assert!(storage.delete_file(file.id).await?);
        assert_eq!(storage.get_file(file.id).await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn upserts_lists_and_deletes_users() -> Result<()> {
        let (_dir, storage) = temp_storage().await?;
        let user = storage
            .upsert_user(NewUser {
                username: "admin".to_string(),
                password_hash: "hash-v1".to_string(),
                can_admin: true,
                can_upload: false,
                can_download: true,
                enabled: true,
            })
            .await?;
        let updated = storage
            .upsert_user(NewUser {
                username: "admin".to_string(),
                password_hash: "hash-v2".to_string(),
                can_admin: true,
                can_upload: true,
                can_download: true,
                enabled: false,
            })
            .await?;

        assert_eq!(user.id, updated.id);
        assert_eq!(updated.password_hash, "hash-v2");
        assert!(updated.can_upload);
        assert!(!updated.enabled);
        assert_eq!(storage.list_users().await?, vec![updated.clone()]);
        assert_eq!(storage.get_user_by_username("admin").await?, Some(updated.clone()));

        assert!(storage.delete_user(updated.id).await?);
        assert_eq!(storage.get_user(updated.id).await?, None);
        Ok(())
    }

    fn new_file(library_id: i64, size: i64) -> NewStoredFile {
        NewStoredFile {
            library_id,
            path: "folder/Game [0100000000001000][v0].nsp".to_string(),
            folder: "folder".to_string(),
            name: "Game [0100000000001000][v0].nsp".to_string(),
            ext: "nsp".to_string(),
            size,
            compressed: false,
            multicontent: false,
            download_count: 0,
            identification_status: IdentificationStatus::Identified,
            title_id: None,
        }
    }

    #[allow(dead_code)]
    fn assert_send_sync()
    where
        Storage: Send + Sync,
        StoredFile: Send + Sync,
    {
    }
}
