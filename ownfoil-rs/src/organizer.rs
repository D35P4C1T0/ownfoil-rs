//! Safe template-based library organization.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::catalog::{ContentFile, ContentKind};
use crate::settings::LibraryManagementSettings;

#[derive(Debug, Error)]
pub enum OrganizerError {
    #[error("organizer I/O error for {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("organizer journal serialization failed: {0}")]
    Journal(#[from] serde_json::Error),
}

#[derive(Debug, Deserialize, Serialize)]
struct JournalEntry {
    source: PathBuf,
    destination: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct MovePreview {
    pub source: PathBuf,
    pub destination: PathBuf,
}

pub fn preview(files: &[ContentFile], settings: &LibraryManagementSettings) -> Vec<MovePreview> {
    files
        .iter()
        .filter_map(|file| {
            let destination = destination_for(file, settings)?;
            (safe_relative(&destination) && destination != file.relative_path)
                .then(|| MovePreview { source: file.relative_path.clone(), destination })
        })
        .collect()
}

pub async fn organize(
    root: &Path,
    files: &[ContentFile],
    settings: &LibraryManagementSettings,
) -> Result<bool, OrganizerError> {
    if !settings.organizer.enabled && !settings.delete_older_updates {
        return Ok(false);
    }
    let root = root.to_path_buf();
    let root_display = root.display().to_string();
    let files = files.to_vec();
    let settings = settings.clone();
    tokio::task::spawn_blocking(move || organize_sync(&root, &files, &settings)).await.map_err(
        |error| OrganizerError::Io {
            path: root_display,
            source: std::io::Error::other(error.to_string()),
        },
    )?
}

fn organize_sync(
    root: &Path,
    files: &[ContentFile],
    settings: &LibraryManagementSettings,
) -> Result<bool, OrganizerError> {
    let mut changed = false;
    changed |= recover_journal(root)?;
    if settings.delete_older_updates {
        changed |= delete_old_updates(root, files)?;
    }
    if !settings.organizer.enabled {
        return Ok(changed);
    }

    let journal_path = root.join(".ownfoil-organizer-journal.json");
    for file in files {
        let Some(destination_relative) = destination_for(file, settings) else {
            continue;
        };
        if !safe_relative(&destination_relative) || destination_relative == file.relative_path {
            continue;
        }
        let source = root.join(&file.relative_path);
        let mut destination = root.join(&destination_relative);
        if !source.is_file() {
            continue;
        }
        destination = collision_destination(&source, destination);
        let journal = JournalEntry { source: source.clone(), destination: destination.clone() };
        write_atomic(&journal_path, &serde_json::to_vec_pretty(&journal)?)?;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        }
        move_file(&source, &destination)?;
        changed = true;
        if journal_path.exists() {
            std::fs::remove_file(&journal_path)
                .map_err(|source| io_error(&journal_path, source))?;
        }
    }
    if settings.organizer.remove_empty_folders {
        remove_empty_folders(root)?;
    }
    Ok(changed)
}

#[allow(clippy::literal_string_with_formatting_args)]
fn destination_for(file: &ContentFile, settings: &LibraryManagementSettings) -> Option<PathBuf> {
    let app_id = file.title_id.as_deref()?;
    let version = file.version.unwrap_or(0).to_string();
    let stem =
        Path::new(&file.name).file_stem().and_then(|value| value.to_str()).unwrap_or("Unknown");
    let title_name = stem.split('[').next().unwrap_or(stem).trim();
    let template = if file.is_multicontent() {
        &settings.organizer.templates.multi
    } else {
        match file.kind {
            ContentKind::Base => &settings.organizer.templates.base,
            ContentKind::Update => &settings.organizer.templates.update,
            ContentKind::Dlc => &settings.organizer.templates.dlc,
            ContentKind::Unknown => &settings.organizer.templates.multi,
        }
    };
    let rendered = template
        .replace("{titleName}", &sanitize(title_name, settings.organizer.windows_compatible))
        .replace("{appName}", &sanitize(title_name, settings.organizer.windows_compatible))
        .replace("{titleId}", app_id)
        .replace("{appId}", app_id)
        .replace("{appVersion}", &version);
    let extension =
        file.relative_path.extension().and_then(|value| value.to_str()).unwrap_or_default();
    let rendered = rendered
        .replace("{extension}", extension)
        .replace("{patchLevel}", &(file.version.unwrap_or(0) / 65_536).to_string());
    let mut destination = PathBuf::from(rendered);
    if destination.extension().is_none() && !extension.is_empty() {
        destination.set_extension(extension);
    }
    Some(destination)
}

fn collision_destination(source: &Path, destination: PathBuf) -> PathBuf {
    if !destination.exists() || destination == source {
        return destination;
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new(""));
    let stem = destination.file_stem().and_then(|value| value.to_str()).unwrap_or("file");
    let extension = destination.extension().and_then(|value| value.to_str());
    for counter in 2_u32.. {
        let name = extension.map_or_else(
            || format!("{stem}({counter})"),
            |extension| format!("{stem}({counter}).{extension}"),
        );
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn recover_journal(root: &Path) -> Result<bool, OrganizerError> {
    let journal_path = root.join(".ownfoil-organizer-journal.json");
    if !journal_path.is_file() {
        return Ok(false);
    }
    let entry: JournalEntry = serde_json::from_slice(
        &std::fs::read(&journal_path).map_err(|source| io_error(&journal_path, source))?,
    )?;
    let inside_root = entry.source.starts_with(root) && entry.destination.starts_with(root);
    if !inside_root {
        return Err(io_error(
            &journal_path,
            std::io::Error::other("organizer journal escapes library root"),
        ));
    }
    let recovered = if entry.source.is_file() && !entry.destination.exists() {
        if let Some(parent) = entry.destination.parent() {
            std::fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        }
        move_file(&entry.source, &entry.destination)?;
        true
    } else if !entry.source.exists() && entry.destination.is_file() {
        if let Some(parent) = entry.source.parent() {
            std::fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        }
        move_file(&entry.destination, &entry.source)?;
        true
    } else {
        false
    };
    std::fs::remove_file(&journal_path).map_err(|source| io_error(&journal_path, source))?;
    Ok(recovered)
}

pub fn sanitize(name: &str, windows_compatible: bool) -> String {
    let invalid = |character: char| {
        character == '/'
            || character == '\0'
            || (windows_compatible
                && matches!(character, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
    };
    let mut sanitized = name
        .chars()
        .map(|character| if invalid(character) { '_' } else { character })
        .collect::<String>();
    if windows_compatible {
        sanitized = sanitized.trim_end_matches([' ', '.']).to_string();
        let stem = sanitized.split('.').next().unwrap_or_default().to_ascii_lowercase();
        let reserved = matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
            || (stem.len() == 4
                && (stem.starts_with("com") || stem.starts_with("lpt"))
                && stem[3..].parse::<u8>().is_ok_and(|number| (1..=9).contains(&number)));
        if reserved {
            sanitized.insert(0, '_');
        }
    }
    if sanitized.is_empty() { "Unknown".to_string() } else { sanitized }
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path.components().all(|component| matches!(component, Component::Normal(_)))
}

fn move_file(source: &Path, destination: &Path) -> Result<(), OrganizerError> {
    match std::fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(rename_error) if rename_error.raw_os_error() == Some(18) => {
            std::fs::copy(source, destination).map_err(|source| io_error(destination, source))?;
            let copied =
                std::fs::metadata(destination).map_err(|source| io_error(destination, source))?;
            let original = std::fs::metadata(source).map_err(|error| io_error(source, error))?;
            if copied.len() != original.len() {
                return Err(io_error(
                    destination,
                    std::io::Error::other("cross-device copy size mismatch"),
                ));
            }
            std::fs::remove_file(source).map_err(|error| io_error(source, error))
        }
        Err(error) => Err(io_error(source, error)),
    }
}

fn delete_old_updates(root: &Path, files: &[ContentFile]) -> Result<bool, OrganizerError> {
    let mut updates: BTreeMap<String, Vec<&ContentFile>> = BTreeMap::new();
    for file in files.iter().filter(|file| file.kind == ContentKind::Update) {
        if let Some(app_id) = &file.title_id {
            updates.entry(app_id.clone()).or_default().push(file);
        }
    }
    let mut changed = false;
    let journal_path = root.join(".ownfoil-organizer-journal.json");
    let trash_dir = root.join(".ownfoil-trash");
    for versions in updates.values_mut() {
        versions.sort_by_key(|file| std::cmp::Reverse(file.version.unwrap_or(0)));
        for old in versions.iter().skip(1) {
            let path = root.join(&old.relative_path);
            if path.is_file() {
                std::fs::create_dir_all(&trash_dir)
                    .map_err(|source| io_error(&trash_dir, source))?;
                let trash = trash_dir.join(format!("{}.deleted", uuid::Uuid::new_v4()));
                let journal = JournalEntry { source: path.clone(), destination: trash.clone() };
                write_atomic(&journal_path, &serde_json::to_vec_pretty(&journal)?)?;
                move_file(&path, &trash)?;
                std::fs::remove_file(&trash).map_err(|source| io_error(&trash, source))?;
                std::fs::remove_file(&journal_path)
                    .map_err(|source| io_error(&journal_path, source))?;
                changed = true;
            }
        }
    }
    Ok(changed)
}

fn remove_empty_folders(root: &Path) -> Result<(), OrganizerError> {
    for entry in walkdir::WalkDir::new(root)
        .min_depth(1)
        .contents_first(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_dir())
    {
        let path = entry.path();
        let empty =
            std::fs::read_dir(path).map_err(|source| io_error(path, source))?.next().is_none();
        if empty {
            std::fs::remove_dir(path).map_err(|source| io_error(path, source))?;
        }
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), OrganizerError> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes).map_err(|source| io_error(&temp, source))?;
    std::fs::rename(&temp, path).map_err(|source| io_error(path, source))
}

fn io_error(path: &Path, source: std::io::Error) -> OrganizerError {
    OrganizerError::Io { path: path.display().to_string(), source }
}

#[cfg(test)]
mod tests {
    use super::{safe_relative, sanitize};

    #[test]
    fn sanitizes_windows_names_and_rejects_escaping_templates() {
        assert_eq!(sanitize("CON", true), "_CON");
        assert_eq!(sanitize("A:B?", true), "A_B_");
        assert!(safe_relative(std::path::Path::new("Game/Game.nsp")));
        assert!(!safe_relative(std::path::Path::new("../outside.nsp")));
    }
}
