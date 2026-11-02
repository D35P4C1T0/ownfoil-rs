//! Library scanner: recursively walks a directory for `.nsp`, `.xci`, `.nsz`, `.xcz` files.
//!
//! Runs in a blocking task to avoid blocking the async runtime. Parses title ID and
//! version from filenames (e.g. `[0100D2F00D5C0000][v0]`).

use std::ffi::OsStr;
use std::path::Path;

use thiserror::Error;
use tracing::info;
use walkdir::WalkDir;

use crate::catalog::{
    ContentFile, classify_title_id, parse_filename_metadata, to_display_title_id,
};

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("library root does not exist: {0}")]
    MissingRoot(String),
    #[error("failed to walk {path}: {source}")]
    Walk { path: String, source: std::io::Error },
    #[error("failed to read metadata for {path}: {source}")]
    Metadata { path: String, source: std::io::Error },
    #[error("failed to normalize path for {path}")]
    NormalizePath { path: String },
}

/// Recursively scan the library root for supported content files.
///
/// Returns [`ContentFile`] entries with parsed title IDs.
/// Runs in `spawn_blocking` to avoid blocking the async runtime.
pub async fn scan_library(root: &Path) -> Result<Vec<ContentFile>, ScanError> {
    let root_path = root.to_path_buf();
    let path_display = root_path.display().to_string();
    tokio::task::spawn_blocking(move || scan_library_sync(&root_path)).await.map_err(|e| {
        ScanError::Walk { path: path_display, source: std::io::Error::other(e.to_string()) }
    })?
}

fn scan_library_sync(root: &Path) -> Result<Vec<ContentFile>, ScanError> {
    let started_at = std::time::Instant::now();
    if !root.exists() {
        return Err(ScanError::MissingRoot(root.display().to_string()));
    }

    let mut out = Vec::new();

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| ScanError::Walk {
            path: error.path().unwrap_or(root).display().to_string(),
            source: std::io::Error::other(error.to_string()),
        })?;
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }

        if !is_supported_content(path) {
            continue;
        }

        let metadata = std::fs::metadata(path)
            .map_err(|source| ScanError::Metadata { path: path.display().to_string(), source })?;

        let relative_path = path
            .strip_prefix(root)
            .map(Path::to_path_buf)
            .map_err(|_| ScanError::NormalizePath { path: path.display().to_string() })?;

        let name = relative_path
            .file_name()
            .and_then(OsStr::to_str)
            .map_or_else(|| relative_path.display().to_string(), String::from);

        let parsed_name = parse_filename_metadata(&name);
        let rel = relative_path.to_string_lossy();
        let parsed_path = parse_filename_metadata(&rel);

        let title_id = to_display_title_id(parsed_name.title_id.or(parsed_path.title_id));
        let kind = classify_title_id(title_id.as_deref());

        out.push(ContentFile {
            id: 0,
            library_root: root.to_path_buf(),
            relative_path,
            name,
            size: metadata.len(),
            title_id,
            version: parsed_name.version.or(parsed_path.version),
            kind,
            identified_contents: Vec::new(),
        });
    }

    let with_title_id = out.iter().filter(|file| file.title_id.is_some()).count();
    info!(
        root = %root.display(),
        files = out.len(),
        with_title_id,
        elapsed_ms = started_at.elapsed().as_millis(),
        "library scan finished"
    );

    Ok(out)
}

pub fn is_supported_content(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str).is_some_and(|extension| {
        matches!(extension.to_ascii_lowercase().as_str(), "nsp" | "xci" | "nsz" | "xcz")
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use anyhow::Result;
    use tempfile::tempdir;
    use tokio::fs;

    use crate::catalog::ContentKind;

    use super::{is_supported_content, scan_library};

    #[test]
    fn supported_extensions() {
        assert!(is_supported_content(Path::new("game.nsp")));
        assert!(is_supported_content(Path::new("game.xci")));
        assert!(is_supported_content(Path::new("game.nsz")));
        assert!(!is_supported_content(Path::new("game.zip")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn incomplete_scan_preserves_stored_files() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir()?;
        let root = dir.path().join("games");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).await?;
        fs::write(nested.join("game.nsp"), b"content").await?;
        let storage = crate::storage::Storage::open(dir.path().join("test.db")).await?;
        let management = crate::settings::LibraryManagementSettings::default();
        let keys = dir.path().join("absent.keys");
        let files =
            crate::scan_all_libraries(std::slice::from_ref(&root), &storage, &management, &keys)
                .await?;
        let id = i64::try_from(files[0].id)?;
        storage.increment_file_download_count(id).await?;
        fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o000)).await?;
        let readable = std::fs::read_dir(&nested).is_ok();
        let result =
            crate::scan_all_libraries(std::slice::from_ref(&root), &storage, &management, &keys)
                .await;
        fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700)).await?;
        // Root can still read mode-000 directories.
        if !readable {
            assert!(result.is_err());
            assert_eq!(storage.get_file(id).await?.map(|file| file.download_count), Some(1));
        }
        let restored = crate::scan_all_libraries(&[root], &storage, &management, &keys).await?;
        assert_eq!(restored[0].id, files[0].id);
        Ok(())
    }

    #[tokio::test]
    async fn scan_library_detects_dlc_in_nested_directories() -> Result<()> {
        let dir = tempdir()?;
        let nested =
            dir.path().join("Some Game").join("DLC").join("my_dlc_[0100ABCD12341001][v0].nsp");
        if let Some(parent) = nested.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&nested, b"dummy").await?;

        let files = scan_library(dir.path()).await?;
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.title_id.as_deref(), Some("0100ABCD12341001"));
        assert_eq!(file.kind, ContentKind::Dlc);
        Ok(())
    }

    #[tokio::test]
    async fn scan_library_parses_title_id_from_parent_directory_path() -> Result<()> {
        let dir = tempdir()?;
        let nested = dir.path().join("Game_[0100ABCD12340000]").join("content").join("file.nsp");
        if let Some(parent) = nested.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&nested, b"dummy").await?;

        let files = scan_library(dir.path()).await?;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].title_id.as_deref(), Some("0100ABCD12340000"));
        assert_eq!(files[0].kind, ContentKind::Base);
        Ok(())
    }
}
