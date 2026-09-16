//! Persistent `SQLite` storage for Ownfoil concepts.
//!
//! This module is intentionally not wired into HTTP state yet. It defines the
//! durable schema and a small async-friendly API that route handlers can adopt
//! incrementally.
#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::catalog::{ContentFile, ContentKind, IdentifiedContent};

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
#[allow(clippy::struct_field_names, clippy::struct_excessive_bools)]
pub struct Title {
    pub title_id: String,
    pub have_base: bool,
    pub up_to_date: bool,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
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
#[allow(clippy::struct_excessive_bools)]
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
#[allow(clippy::struct_excessive_bools)]
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
    #[error("storage I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, StorageError>;

impl Storage {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db_path = path.as_ref().to_path_buf();
        let init_path = db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = open_connection(&init_path)?;
            if table_has_column(&conn, "files", "filepath")? {
                backup_before_migration(&init_path)?;
            }
            initialize_schema(&mut conn)
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
                r"
                INSERT INTO libraries (path, last_scan)
                VALUES (?1, ?2)
                ON CONFLICT(path) DO UPDATE SET
                    last_scan = excluded.last_scan
                ",
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

    /// Resolve user and extracted metadata independently so deleting a custom
    /// override restores extracted fields instead of deleting both sources.
    pub async fn title_override_records(&self) -> Result<Vec<(String, serde_json::Value)>> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT title_id,record FROM extracted_title_overrides UNION ALL SELECT title_id,record FROM title_overrides")?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?)))?;
            let mut records = std::collections::BTreeMap::<String, serde_json::Map<String, serde_json::Value>>::new();
            for row in rows {
                let (id, raw) = row?;
                let record: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw).map_err(|error| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(error)))?;
                let merged = records.entry(id).or_default();
                for (key,value) in record { if !value.is_null() { merged.insert(key,value); } }
            }
            Ok(records.into_iter().map(|(id,record)|(id,serde_json::Value::Object(record))).collect())
        }).await
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
                r"
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
                ",
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
                r"
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
                ",
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

    /// Reconcile one complete scanner snapshot into durable storage.
    ///
    /// File IDs survive rescans. Rows absent from the new snapshot are removed in
    /// the same transaction, then returned files carry their durable IDs and root.
    #[allow(clippy::too_many_lines)]
    pub async fn reconcile_library_scan(
        &self,
        root: PathBuf,
        mut files: Vec<ContentFile>,
    ) -> Result<Vec<ContentFile>> {
        self.with_connection(move |conn| {
            let transaction = conn.transaction()?;
            let root_text = root.to_string_lossy().into_owned();
            let scanned_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|duration| i64::try_from(duration.as_secs()).ok());
            transaction.execute(
                r"
                INSERT INTO libraries (path, last_scan)
                VALUES (?1, ?2)
                ON CONFLICT(path) DO UPDATE SET last_scan = excluded.last_scan
                ",
                params![root_text, scanned_at],
            )?;
            let library_id: i64 = transaction.query_row(
                "SELECT id FROM libraries WHERE path = ?1",
                params![root_text],
                |row| row.get(0),
            )?;

            let mut present_paths = HashSet::with_capacity(files.len());
            for file in &mut files {
                let relative_path = file.relative_path.to_string_lossy().into_owned();
                present_paths.insert(relative_path.clone());
                let folder = file
                    .relative_path
                    .parent()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let extension = file
                    .relative_path
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let size = i64::try_from(file.size).unwrap_or(i64::MAX);
                let compressed = matches!(extension.as_str(), "nsz" | "xcz");
                let status = if file.title_id.is_some() { "identified" } else { "unknown" };
                let identification_error =
                    file.title_id.is_none().then_some("Could not determine App ID from filename");
                let identification_type =
                    if file.identified_contents.is_empty() { "filename" } else { "cnmt" };
                let content_count = if file.identified_contents.is_empty() {
                    usize::from(file.title_id.is_some())
                } else {
                    file.identified_contents.len()
                };
                let stored_title_id = file
                    .identified_contents
                    .first()
                    .map(|identity| identity.title_id.clone())
                    .or_else(|| {
                        file.title_id.as_ref().map(|app_id| base_title_id(app_id, file.kind))
                    });
                if let Some(title_id) = &stored_title_id {
                    transaction.execute(
                        "INSERT INTO titles (title_id) VALUES (?1) ON CONFLICT DO NOTHING",
                        params![title_id],
                    )?;
                }
                transaction.execute(
                    r"
                    INSERT INTO files (
                        library_id, path, folder, name, ext, size, compressed,
                        multicontent, identification_status, title_id,
                        identification_type, identification_error, identification_attempts,
                        last_attempt, nb_content
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                            ?11, ?12, 1, ?13, ?14)
                    ON CONFLICT(library_id, path) DO UPDATE SET
                        folder = excluded.folder,
                        name = excluded.name,
                        ext = excluded.ext,
                        size = excluded.size,
                        compressed = excluded.compressed,
                        identification_status = excluded.identification_status,
                        title_id = excluded.title_id,
                        identification_type = excluded.identification_type,
                        identification_error = excluded.identification_error,
                        identification_attempts = CASE
                            WHEN files.size != excluded.size THEN files.identification_attempts + 1
                            ELSE files.identification_attempts
                        END,
                        last_attempt = CASE
                            WHEN files.size != excluded.size THEN excluded.last_attempt
                            ELSE files.last_attempt
                        END,
                        nb_content = excluded.nb_content
                    ",
                    params![
                        library_id,
                        relative_path,
                        folder,
                        file.name,
                        extension,
                        size,
                        compressed,
                        content_count > 1,
                        status,
                        stored_title_id,
                        identification_type,
                        identification_error,
                        scanned_at,
                        i64::try_from(content_count).unwrap_or(i64::MAX)
                    ],
                )?;
                let id: i64 = transaction.query_row(
                    "SELECT id FROM files WHERE library_id = ?1 AND path = ?2",
                    params![library_id, relative_path],
                    |row| row.get(0),
                )?;
                let modified=std::fs::metadata(root.join(&file.relative_path)).ok().and_then(|m|m.modified().ok()).and_then(|t|t.duration_since(UNIX_EPOCH).ok()).map(|d|d.as_secs_f64());
                transaction.execute("UPDATE files SET signature_valid=CASE WHEN mtime IS NOT ?2 THEN NULL ELSE signature_valid END,hash_valid=CASE WHEN mtime IS NOT ?2 THEN NULL ELSE hash_valid END,hash_modified=CASE WHEN mtime IS NOT ?2 THEN NULL ELSE hash_modified END,verification_error=CASE WHEN mtime IS NOT ?2 THEN NULL ELSE verification_error END,verified_at=CASE WHEN mtime IS NOT ?2 THEN NULL ELSE verified_at END,mtime=?2,added_at=COALESCE(added_at,strftime('%Y-%m-%dT%H:%M:%fZ','now')) WHERE id=?1",params![id,modified])?;
                file.id = usize::try_from(id).unwrap_or(0);
                file.library_root.clone_from(&root);
                transaction.execute("DELETE FROM app_files WHERE file_id = ?1", params![id])?;
                let identities = if file.identified_contents.is_empty() {
                    file.title_id
                        .as_ref()
                        .map(|app_id| {
                            vec![IdentifiedContent {
                                title_id: base_title_id(app_id, file.kind),
                                app_id: app_id.clone(),
                                version: file.version.unwrap_or(0),
                                kind: file.kind,
                            }]
                        })
                        .unwrap_or_default()
                } else {
                    file.identified_contents.clone()
                };
                for identity in identities {
                    let base_id = identity.title_id;
                    transaction.execute(
                        "INSERT INTO titles (title_id) VALUES (?1) ON CONFLICT DO NOTHING",
                        params![base_id],
                    )?;
                    let title_row: i64 = transaction.query_row(
                        "SELECT id FROM titles WHERE title_id = ?1",
                        params![base_id],
                        |row| row.get(0),
                    )?;
                    transaction.execute(
                        r"
                        INSERT INTO apps (title_id, app_id, app_version, app_type, owned)
                        VALUES (?1, ?2, ?3, ?4, 1)
                        ON CONFLICT(app_id, app_version) DO UPDATE SET
                            title_id = excluded.title_id,
                            app_type = excluded.app_type,
                            owned = 1
                        ",
                        params![
                            title_row,
                            identity.app_id,
                            identity.version.to_string(),
                            app_type(identity.kind)
                        ],
                    )?;
                    let app_row: i64 = transaction.query_row(
                        "SELECT id FROM apps WHERE app_id = ?1 AND app_version = ?2",
                        params![identity.app_id, identity.version.to_string()],
                        |row| row.get(0),
                    )?;
                    transaction.execute(
                        "INSERT OR IGNORE INTO app_files (app_id, file_id) VALUES (?1, ?2)",
                        params![app_row, id],
                    )?;
                }
            }

            let mut stale =
                transaction.prepare("SELECT id, path FROM files WHERE library_id = ?1")?;
            let stale_rows = stale.query_map(params![library_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            let stale_ids = stale_rows
                .filter_map(std::result::Result::ok)
                .filter_map(|(id, path)| (!present_paths.contains(&path)).then_some(id))
                .collect::<Vec<_>>();
            drop(stale);
            for id in stale_ids {
                transaction.execute("DELETE FROM files WHERE id = ?1", params![id])?;
            }
            transaction.execute(
                "UPDATE apps SET owned = EXISTS(SELECT 1 FROM app_files WHERE app_id = apps.id)",
                [],
            )?;
            transaction.execute_batch(
                r"
                UPDATE titles SET
                    have_base = EXISTS(
                        SELECT 1 FROM apps WHERE apps.title_id = titles.id
                        AND apps.app_type = 'BASE' AND apps.owned = 1
                    ),
                    up_to_date = CASE
                        WHEN NOT EXISTS(SELECT 1 FROM apps WHERE apps.title_id = titles.id AND apps.app_type = 'UPDATE') THEN 1
                        ELSE COALESCE((
                            SELECT MAX(CAST(app_version AS INTEGER)) FROM apps
                            WHERE apps.title_id = titles.id AND apps.app_type = 'UPDATE' AND owned = 1
                        ), -1) >= COALESCE((
                            SELECT MAX(CAST(app_version AS INTEGER)) FROM apps
                            WHERE apps.title_id = titles.id AND apps.app_type = 'UPDATE'
                        ), 0)
                    END,
                    complete = NOT EXISTS(
                        SELECT 1 FROM apps WHERE apps.title_id = titles.id
                        AND apps.app_type = 'DLC' AND apps.owned = 0
                    );
                ",
            )?;
            transaction.commit()?;
            Ok(files)
        })
        .await
    }

    pub(crate) async fn with_connection<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = open_connection(&db_path)?;
            f(&mut conn)
        })
        .await?
    }
}

impl IdentificationStatus {
    const fn as_str(&self) -> &'static str {
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
    const fn as_str(&self) -> &'static str {
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

const fn app_type(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Base => "BASE",
        ContentKind::Update => "UPDATE",
        ContentKind::Dlc => "DLC",
        ContentKind::Unknown => "UNKNOWN",
    }
}

pub fn base_title_id(app_id: &str, kind: ContentKind) -> String {
    let normalized = app_id.to_ascii_uppercase();
    match kind {
        ContentKind::Base | ContentKind::Unknown => normalized,
        ContentKind::Update => format!("{}000", &normalized[..normalized.len().saturating_sub(3)]),
        ContentKind::Dlc => normalized
            .get(..13)
            .and_then(|prefix| u64::from_str_radix(prefix, 16).ok())
            .and_then(|prefix| prefix.checked_sub(1))
            .map_or(normalized, |prefix| format!("{prefix:013X}000")),
    }
}

fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(conn)
}

fn initialize_schema(conn: &mut Connection) -> Result<()> {
    if table_has_column(conn, "files", "filepath")? {
        migrate_upstream_schema(conn)?;
    } else {
        create_schema(conn)?;
    }
    Ok(())
}

fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS tasks (
            id INTEGER PRIMARY KEY AUTOINCREMENT, task_name TEXT NOT NULL,
            input_json TEXT NOT NULL DEFAULT '{}', output_json TEXT,
            status TEXT NOT NULL DEFAULT 'pending', completion_pct INTEGER NOT NULL DEFAULT 0,
            exit_code INTEGER, error_message TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
            started_at TEXT, completed_at TEXT, run_after TEXT, parent_id INTEGER,
            worker_id INTEGER, cancel_requested INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS title_overrides (title_id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS extracted_title_overrides (title_id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS libraries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL UNIQUE,
            last_scan INTEGER
        );

        CREATE TABLE IF NOT EXISTS titles (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title_id TEXT NOT NULL UNIQUE,
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
            identification_type TEXT,
            identification_error TEXT,
            identification_attempts INTEGER NOT NULL DEFAULT 0,
            last_attempt INTEGER,
            nb_content INTEGER NOT NULL DEFAULT 0,
            UNIQUE(library_id, path)
        );

        CREATE TABLE IF NOT EXISTS apps (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title_id INTEGER NOT NULL REFERENCES titles(id) ON DELETE CASCADE,
            app_id TEXT NOT NULL,
            app_version TEXT NOT NULL,
            app_type TEXT NOT NULL,
            owned INTEGER NOT NULL DEFAULT 0,
            UNIQUE(app_id, app_version)
        );

        CREATE TABLE IF NOT EXISTS app_files (
            app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
            file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            PRIMARY KEY (app_id, file_id)
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
        ",
    )?;
    ensure_schema_columns(conn)
}

fn ensure_schema_columns(conn: &Connection) -> rusqlite::Result<()> {
    for (name, definition) in [
        ("identification_type", "TEXT"),
        ("identification_error", "TEXT"),
        ("identification_attempts", "INTEGER NOT NULL DEFAULT 0"),
        ("last_attempt", "INTEGER"),
        ("signature_valid", "INTEGER"),
        ("hash_valid", "INTEGER"),
        ("hash_modified", "INTEGER"),
        ("verification_error", "TEXT"),
        ("verified_at", "TEXT"),
        ("mtime", "REAL"),
        ("added_at", "TEXT"),
        ("organized", "INTEGER NOT NULL DEFAULT 0"),
        ("nb_content", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !table_has_column(conn, "files", name)? {
            conn.execute_batch(&format!("ALTER TABLE files ADD COLUMN {name} {definition};"))?;
        }
    }
    if !table_has_column(conn, "tasks", "cancel_requested")? {
        conn.execute(
            "ALTER TABLE tasks ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !table_has_column(conn, "tasks", "rerun_requested")? {
        conn.execute(
            "ALTER TABLE tasks ADD COLUMN rerun_requested INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);\
         INSERT INTO schema_version(version) SELECT 3 WHERE NOT EXISTS(SELECT 1 FROM schema_version);\
         UPDATE schema_version SET version = 3;",
    )
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing?.eq_ignore_ascii_case(column) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn backup_before_migration(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let backup = path.with_file_name(format!(".backup_ownfoil_{}.db", uuid::Uuid::new_v4()));
    // VACUUM INTO includes committed WAL pages, unlike copying only the .db file.
    let connection = Connection::open(path)?;
    connection.execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()])?;
    Ok(())
}

#[allow(clippy::too_many_lines)] // Keep schema migration atomic and reviewable in one place.
fn migrate_upstream_schema(conn: &mut Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let transaction = conn.transaction()?;
    transaction.execute_batch(
        r"
        ALTER TABLE libraries RENAME TO upstream_libraries;
        ALTER TABLE files RENAME TO upstream_files;
        ALTER TABLE titles RENAME TO upstream_titles;
        ALTER TABLE apps RENAME TO upstream_apps;
        ALTER TABLE app_files RENAME TO upstream_app_files;
        ALTER TABLE users RENAME TO upstream_users;
        ",
    )?;
    let migrate_tasks = table_has_column(&transaction, "tasks", "input_hash")?;
    if migrate_tasks {
        transaction.execute("ALTER TABLE tasks RENAME TO upstream_tasks", [])?;
    }
    let migrate_overrides = table_has_column(&transaction, "title_overrides", "source")?;
    if migrate_overrides {
        transaction
            .execute("ALTER TABLE title_overrides RENAME TO upstream_title_overrides", [])?;
    }
    create_schema(&transaction)?;
    transaction.execute_batch(
        r"
        INSERT INTO libraries (id, path, last_scan)
        SELECT id, path, CAST(strftime('%s', last_scan) AS INTEGER)
        FROM upstream_libraries;

        INSERT INTO titles (id, title_id, have_base, up_to_date, complete)
        SELECT id, title_id, have_base, up_to_date, complete
        FROM upstream_titles
        WHERE title_id IS NOT NULL;

        INSERT INTO files (
            id, library_id, path, folder, name, ext, size, compressed,
            multicontent, download_count, identification_status, title_id,
            identification_type, identification_error, identification_attempts,
            last_attempt, nb_content
        )
        SELECT
            f.id,
            f.library_id,
            CASE
                WHEN instr(f.filepath, l.path) = 1
                THEN ltrim(substr(f.filepath, length(l.path) + 1), '/')
                ELSE f.filename
            END,
            f.folder,
            f.filename,
            f.extension,
            f.size,
            f.compressed,
            f.multicontent,
            f.download_count,
            CASE
                WHEN f.identified = 1 THEN 'identified'
                WHEN f.identification_error IS NOT NULL THEN 'failed'
                ELSE 'unknown'
            END,
            NULL,
            f.identification_type,
            f.identification_error,
            f.identification_attempts,
            CAST(strftime('%s', f.last_attempt) AS INTEGER),
            f.nb_content
        FROM upstream_files f
        JOIN upstream_libraries l ON l.id = f.library_id;

        INSERT INTO apps (id, title_id, app_id, app_version, app_type, owned)
        SELECT
            a.id,
            a.title_id,
            a.app_id,
            a.app_version,
            a.app_type,
            a.owned
        FROM upstream_apps a
        JOIN upstream_titles t ON t.id = a.title_id;

        INSERT INTO app_files (app_id, file_id)
        SELECT app_id, file_id FROM upstream_app_files;

        INSERT INTO users (
            id, username, password_hash, can_admin, can_upload, can_download, enabled
        )
        SELECT id, user, password, admin_access, backup_access, shop_access, 1
        FROM upstream_users;

        ",
    )?;
    for column in [
        "organized",
        "signature_valid",
        "hash_valid",
        "hash_modified",
        "verification_error",
        "verified_at",
        "mtime",
        "added_at",
    ] {
        if table_has_column(&transaction, "upstream_files", column)? {
            transaction.execute_batch(&format!("UPDATE files SET {column}=(SELECT {column} FROM upstream_files WHERE upstream_files.id=files.id)"))?;
        }
    }
    if migrate_tasks {
        transaction.execute_batch("INSERT INTO tasks(id,parent_id,task_name,status,completion_pct,input_json,output_json,exit_code,error_message,run_after,created_at,started_at,completed_at,worker_id) SELECT id,parent_id,task_name,status,COALESCE(completion_pct,0),input_json,output_json,exit_code,error_message,strftime('%Y-%m-%dT%H:%M:%fZ',run_after),strftime('%Y-%m-%dT%H:%M:%fZ',created_at),strftime('%Y-%m-%dT%H:%M:%fZ',started_at),strftime('%Y-%m-%dT%H:%M:%fZ',completed_at),worker_id FROM upstream_tasks; DROP TABLE upstream_tasks;")?;
    }
    if migrate_overrides {
        let mut stmt=transaction.prepare("SELECT * FROM upstream_title_overrides ORDER BY CASE source WHEN 'custom' THEN 0 ELSE 1 END")?;
        let columns = stmt.column_names().iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let rows = stmt
            .query_map([], |row| {
                let mut record = serde_json::Map::new();
                for (index, column) in columns.iter().enumerate() {
                    if matches!(column.as_str(), "id" | "source") {
                        continue;
                    }
                    if let Some(value) = row.get::<_, Option<String>>(index)? {
                        let mut parts = column.split('_');
                        let mut name = parts.next().unwrap_or_default().to_string();
                        for part in parts {
                            let mut chars = part.chars();
                            if let Some(first) = chars.next() {
                                name.extend(first.to_uppercase());
                                name.extend(chars);
                            }
                        }
                        if column == "nca_key" {
                            name = "key".into();
                        }
                        let value = if matches!(
                            column.as_str(),
                            "category"
                                | "rating_content"
                                | "regions"
                                | "languages"
                                | "screenshots"
                                | "ids"
                        ) {
                            serde_json::from_str(&value).unwrap_or(serde_json::Value::Null)
                        } else {
                            value.into()
                        };
                        record.insert(name, value);
                    }
                }
                Ok((row.get::<_, String>("id")?, row.get::<_, String>("source")?, record))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, source, record) in rows {
            let table =
                if source == "extract" { "extracted_title_overrides" } else { "title_overrides" };
            transaction.execute(
                &format!("INSERT INTO {table}(title_id,record) VALUES(?1,?2)"),
                params![id, serde_json::Value::Object(record).to_string()],
            )?;
        }
        transaction.execute("DROP TABLE upstream_title_overrides", [])?;
    }
    transaction.execute_batch(
        r"
        DROP TABLE upstream_app_files;
        DROP TABLE upstream_apps;
        DROP TABLE upstream_files;
        DROP TABLE upstream_titles;
        DROP TABLE upstream_libraries;
        DROP TABLE upstream_users;
        ",
    )?;
    transaction.commit()?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(())
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
        r"
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
        "
    )
}

fn user_select(clause: &str) -> String {
    format!(
        r"
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
        "
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::catalog::{ContentFile, ContentKind};

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

    #[tokio::test]
    async fn reconciles_scan_with_stable_ids_and_removes_missing_files() -> Result<()> {
        let (_dir, storage) = temp_storage().await?;
        let root = PathBuf::from("/games");
        let first = storage
            .reconcile_library_scan(
                root.clone(),
                vec![content_file("a.nsp", 10), content_file("b.nsp", 20)],
            )
            .await?;
        let a_id = first[0].id;
        let b_id = first[1].id;
        assert_ne!(a_id, 0);
        assert_ne!(a_id, b_id);

        let second =
            storage.reconcile_library_scan(root.clone(), vec![content_file("a.nsp", 30)]).await?;
        assert_eq!(second[0].id, a_id);
        assert_eq!(second[0].library_root, root);
        assert_eq!(storage.get_file(i64::try_from(b_id).unwrap_or_default()).await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn imports_upstream_schema_with_ids_users_and_backup() -> Result<()> {
        let dir = tempfile::tempdir()
            .map_err(|source| rusqlite::Error::ToSqlConversionFailure(Box::new(source)))?;
        let path = dir.path().join("ownfoil.db");
        {
            let conn = rusqlite::Connection::open(&path)?;
            conn.execute_batch(
                r#"
                CREATE TABLE libraries (id INTEGER PRIMARY KEY, path TEXT, last_scan DATETIME);
                CREATE TABLE files (
                    id INTEGER PRIMARY KEY, library_id INTEGER, filepath TEXT, folder TEXT,
                    filename TEXT, extension TEXT, size INTEGER, compressed BOOLEAN,
                    multicontent BOOLEAN, nb_content INTEGER, download_count INTEGER,
                    identified BOOLEAN, identification_type TEXT, identification_error TEXT,
                    identification_attempts INTEGER, last_attempt DATETIME
                );
                CREATE TABLE titles (
                    id INTEGER PRIMARY KEY, title_id TEXT UNIQUE, have_base BOOLEAN,
                    up_to_date BOOLEAN, complete BOOLEAN
                );
                CREATE TABLE apps (
                    id INTEGER PRIMARY KEY, title_id INTEGER, app_id TEXT, app_version TEXT,
                    app_type TEXT, owned BOOLEAN
                );
                CREATE TABLE app_files (app_id INTEGER, file_id INTEGER);
                CREATE TABLE users (
                    id INTEGER PRIMARY KEY, user TEXT, password TEXT, admin_access BOOLEAN,
                    shop_access BOOLEAN, backup_access BOOLEAN
                );
                INSERT INTO libraries VALUES (4, '/games', NULL);
                INSERT INTO titles VALUES (7, '0100000000000000', 1, 1, 1);
                INSERT INTO files VALUES (
                    42, 4, '/games/Demo [0100000000000000][v0].nsp', '/games',
                    'Demo [0100000000000000][v0].nsp', 'nsp', 123, 0, 0, 1, 9, 1,
                    'cnmt', NULL, 1, NULL
                );
                INSERT INTO apps VALUES (8, 7, '0100000000000000', '0', 'BASE', 1);
                INSERT INTO app_files VALUES (8, 42);
                INSERT INTO users VALUES (3, 'admin', '$scrypt$hash', 1, 1, 1);
                ALTER TABLE files ADD COLUMN signature_valid BOOLEAN;
                ALTER TABLE files ADD COLUMN hash_valid BOOLEAN;
                ALTER TABLE files ADD COLUMN verified_at DATETIME;
                UPDATE files SET signature_valid=1,hash_valid=1,verified_at='2026-09-15 10:00:00';
                CREATE TABLE tasks (id INTEGER PRIMARY KEY,parent_id INTEGER,task_name TEXT,status TEXT,completion_pct INTEGER,input_json TEXT,input_hash TEXT,output_json TEXT,exit_code INTEGER,error_message TEXT,run_after DATETIME,created_at DATETIME,started_at DATETIME,completed_at DATETIME,worker_id INTEGER);
                INSERT INTO tasks VALUES(9,NULL,'verify_file','pending',0,'{}','hash',NULL,NULL,NULL,'2026-09-16 10:00:00','2026-09-15 10:00:00',NULL,NULL,NULL);
                CREATE TABLE title_overrides(id TEXT,source TEXT,name TEXT,languages TEXT,PRIMARY KEY(id,source));
                INSERT INTO title_overrides VALUES('0100000000000000','extract','Extracted title','["en"]');
                INSERT INTO title_overrides VALUES('0100000000000000','custom','Custom title',NULL);
                "#,
            )?;
        }

        let storage = Storage::open(&path).await?;
        let file = storage.get_file(42).await?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        assert_eq!(file.path, "Demo [0100000000000000][v0].nsp");
        assert_eq!(file.download_count, 9);
        let user = storage.get_user(3).await?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        assert_eq!(user.username, "admin");
        assert!(user.can_admin);
        storage
            .with_connection(|conn| {
                let verdict: (i64, i64) = conn.query_row(
                    "SELECT signature_valid,hash_valid FROM files WHERE id=42",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                assert_eq!(verdict, (1, 1));
                let deadline: String =
                    conn.query_row("SELECT run_after FROM tasks WHERE id=9", [], |row| row.get(0))?;
                assert_eq!(deadline, "2026-09-16T10:00:00.000Z");
                let record: String = conn.query_row(
                    "SELECT record FROM title_overrides WHERE title_id='0100000000000000'",
                    [],
                    |row| row.get(0),
                )?;
                assert!(record.contains("Custom title"));

                Ok(())
            })
            .await?;

        let merged = storage.title_override_records().await?;
        assert_eq!(merged[0].1["name"], "Custom title");
        assert_eq!(merged[0].1["languages"], serde_json::json!(["en"]));
        storage
            .with_connection(|conn| {
                conn.execute("DELETE FROM title_overrides", [])?;
                Ok(())
            })
            .await?;
        let restored = storage.title_override_records().await?;
        assert_eq!(restored[0].1["name"], "Extracted title");
        let has_backup = std::fs::read_dir(dir.path())
            .map_err(|source| rusqlite::Error::ToSqlConversionFailure(Box::new(source)))?
            .filter_map(std::result::Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".backup_ownfoil_"));
        assert!(has_backup);
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

    fn content_file(name: &str, size: u64) -> ContentFile {
        ContentFile {
            id: 0,
            library_root: PathBuf::new(),
            relative_path: PathBuf::from(name),
            name: name.to_string(),
            size,
            title_id: Some("0100000000000000".to_string()),
            version: Some(0),
            kind: ContentKind::Base,
            identified_contents: Vec::new(),
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
