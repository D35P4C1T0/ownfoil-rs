//! CNMT-based identification for Nintendo Switch containers.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use nx_archive::TitleDataExt;
use nx_archive::formats::Keyset;
use nx_archive::formats::cnmt::{Cnmt, ContentMetaType, ExtendedHeader};
use nx_archive::formats::pfs0::Pfs0;
use nx_archive::formats::xci::Xci;
use tracing::{debug, warn};

use crate::catalog::{ContentFile, ContentKind, IdentifiedContent};

#[derive(Clone)]
struct CloneReader(Arc<Mutex<File>>);

impl CloneReader {
    fn open(path: &Path) -> std::io::Result<Self> {
        File::open(path).map(|file| Self(Arc::new(Mutex::new(file))))
    }

    fn locked(&self) -> std::io::Result<std::sync::MutexGuard<'_, File>> {
        self.0.lock().map_err(|_| std::io::Error::other("container reader lock poisoned"))
    }
}

impl Read for CloneReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.locked()?.read(buffer)
    }
}

impl Seek for CloneReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.locked()?.seek(position)
    }
}

pub async fn identify_files(
    root: &Path,
    files: Vec<ContentFile>,
    keys_path: &Path,
) -> Vec<ContentFile> {
    if !keys_path.is_file() || crate::keys::inspect(keys_path).valid_keys != Some(true) {
        return files;
    }
    let Ok(keyset) = Keyset::from_file(keys_path) else {
        return files;
    };
    let root = Arc::new(root.to_path_buf());
    let keyset = Arc::new(keyset);
    let mut identified = futures_util::stream::iter(files.into_iter().enumerate())
        .map(|(index, mut file)| {
            let root = Arc::clone(&root);
            let keyset = Arc::clone(&keyset);
            let fallback = file.clone();
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    let path = root.join(&file.relative_path);
                    match identify_container(&path, &keyset) {
                        Ok(contents) if !contents.is_empty() => {
                            if let Some(primary) = contents.first() {
                                file.title_id = Some(primary.app_id.clone());
                                file.version = Some(primary.version);
                                file.kind = primary.kind;
                            }
                            file.identified_contents = contents;
                        }
                        Ok(_) => debug!(path = %path.display(), "no CNMT records; filename metadata retained"),
                        Err(error) => {
                            debug!(path = %path.display(), error = %error, "CNMT unavailable; filename metadata retained");
                        }
                    }
                    file
                })
                .await;
                match result {
                    Ok(file) => (index, file),
                    Err(error) => {
                        warn!(error = %error, "CNMT identification task failed; filename metadata retained");
                        (index, fallback)
                    }
                }
            }
        })
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
    identified.sort_by_key(|(index, _)| *index);
    identified.into_iter().map(|(_, file)| file).collect()
}

fn identify_container(path: &Path, keyset: &Keyset) -> Result<Vec<IdentifiedContent>, String> {
    let reader = CloneReader::open(path).map_err(|error| error.to_string())?;
    let extension =
        path.extension().and_then(|value| value.to_str()).unwrap_or_default().to_ascii_lowercase();
    let cnmts = match extension.as_str() {
        "nsp" | "nsz" => {
            let mut container = Pfs0::from_reader(reader).map_err(|error| error.to_string())?;
            container.get_cnmts(keyset, None).map_err(|error| error.to_string())?
        }
        "xci" | "xcz" => {
            let mut container = Xci::new(reader).map_err(|error| error.to_string())?;
            container.get_cnmts(keyset, None).map_err(|error| error.to_string())?
        }
        _ => return Ok(Vec::new()),
    };
    Ok(cnmts.iter().filter_map(identity_from_cnmt).collect())
}

fn identity_from_cnmt(cnmt: &Cnmt) -> Option<IdentifiedContent> {
    let app_id = format!("{:016X}", cnmt.header.title_id);
    let (kind, title_id) = match (cnmt.header.meta_type, &cnmt.extended_header) {
        (ContentMetaType::Application, _) => (ContentKind::Base, app_id.clone()),
        (ContentMetaType::Patch, ExtendedHeader::Patch(header)) => {
            (ContentKind::Update, format!("{:016X}", header.application_id))
        }
        (ContentMetaType::AddOnContent, ExtendedHeader::Addon(header)) => {
            (ContentKind::Dlc, format!("{:016X}", header.application_id))
        }
        _ => return None,
    };
    Some(IdentifiedContent { title_id, app_id, version: cnmt.header.title_version, kind })
}
