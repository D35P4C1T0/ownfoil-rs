#![allow(clippy::case_sensitive_file_extension_comparisons)] // NCA member names are case-sensitive format identifiers.
//! Native Switch container conversion and verification. No external runtimes.
mod archive;
mod ncz;
mod public_moduli;
pub mod verification;
use crate::http::AppState;
use anyhow::{Context, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn roundtrip(source: &Path, target: &Path) -> anyhow::Result<()> {
    let (mut source_file, source_root) = archive::root(source)?;
    let (mut target_file, target_root) = archive::root(target)?;
    let source_entries = archive::leaves(&mut source_file, &source_root)?;
    let target_entries = archive::leaves(&mut target_file, &target_root)?;
    let normalized = |name: &str| {
        name.strip_suffix(".ncz").map_or_else(|| name.to_string(), |stem| format!("{stem}.nca"))
    };
    ensure!(source_entries.len() == target_entries.len(), "Round-trip member count mismatch");
    for entry in &source_entries {
        let target_entry = target_entries
            .iter()
            .find(|candidate| normalized(&candidate.name) == normalized(&entry.name))
            .context("Round-trip missing member")?;
        ensure!(
            verification::member_hash(&source_file, entry)?
                == verification::member_hash(&target_file, target_entry)?,
            "Round-trip content hash mismatch: {}",
            entry.name
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // Keep conversion publication and recovery stages together.
pub async fn run(
    state: &AppState,
    task_id: i64,
    name: &str,
    input: &Value,
) -> anyhow::Result<Value> {
    let storage = state.storage.as_ref().context("Storage unavailable")?;
    let file_id = input["file_id"]
        .as_i64()
        .or_else(|| input["file_id"].as_str()?.parse().ok())
        .context("file_id is required")?;
    let file = state
        .catalog
        .read()
        .await
        .files()
        .iter()
        .find(|file| i64::try_from(file.id).ok() == Some(file_id))
        .cloned()
        .context("File not found")?;
    let root = std::fs::canonicalize(&file.library_root)?;
    let source = std::fs::canonicalize(root.join(&file.relative_path))?;
    ensure!(source.starts_with(&root), "File outside library root");
    ensure!(
        crate::keys::inspect(&state.keys_path).valid_keys == Some(true),
        "No valid console keys loaded"
    );
    let management = state.settings.read().await.library.management.clone();
    let keys_path = state.keys_path.clone();
    let source_copy = source.clone();
    if name == "verify_file" {
        let before = std::fs::metadata(&source)?;
        let depth = management.verification.depth.clone();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_work = cancelled.clone();
        let (progress_tx, mut progress_rx) = tokio::sync::watch::channel(0u64);
        let mut work = tokio::task::spawn_blocking(move || {
            let keys = nx_archive::formats::Keyset::from_file(keys_path)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            verification::verify_with_progress(&source_copy, &keys, &depth, &mut |percent| {
                if cancel_work.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(std::io::Error::other("Task cancelled"));
                }
                progress_tx.send_if_modified(|previous| {
                    if *previous == percent {
                        return false;
                    }
                    *previous = percent;
                    true
                });
                Ok(())
            })
        });
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        let result = loop {
            tokio::select! {
                result = &mut work => break result?,
                _ = interval.tick() => {
                    if crate::tasks::cancelled(storage, task_id).await {
                        cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let percent = *progress_rx.borrow_and_update();
                    crate::tasks::progress(storage, task_id, i64::try_from(percent)?).await?;
                }
            }
        };
        let value=result.unwrap_or_else(|error|json!({"signatureValid":false,"hashValid":if management.verification.depth=="hash"{Some(false)}else{None},"hashModified":if management.verification.depth=="hash"{Some(false)}else{None},"verificationError":error.to_string()}));
        let _guard = state.scan_lock.lock().await;
        ensure!(!crate::tasks::cancelled(storage, task_id).await, "Task cancelled");
        let after = std::fs::metadata(&source)?;
        ensure!(
            before.len() == after.len() && before.modified()? == after.modified()?,
            "File changed during verification; scan and retry"
        );
        save_verification(storage, file_id, value.clone()).await?;
        if management.compression.enabled
            && !matches!(
                file.relative_path.extension().and_then(|s| s.to_str()),
                Some("nsz" | "xcz")
            )
            && crate::content::verification::status(
                value["signatureValid"].as_bool(),
                value["hashValid"].as_bool(),
                value["hashModified"].as_bool(),
            ) != "CORRUPT"
        {
            crate::tasks::enqueue(storage, "compress_file", json!({"file_id":file_id})).await?;
        }
        return Ok(value);
    }
    let extension =
        source.extension().and_then(|s| s.to_str()).unwrap_or_default().to_ascii_lowercase();
    let compress = name == "compress_file";
    let new_extension = match (compress, extension.as_str()) {
        (true, "nsp") => "nsz",
        (true, "xci") => "xcz",
        (false, "nsz") => "nsp",
        (false, "xcz") => "xci",
        _ => bail!("File cannot be converted in this direction"),
    };
    let target = source.with_extension(new_extension);
    ensure!(!target.exists(), "Conversion target already exists");
    let temporary = Temporary(
        source.with_file_name(format!(".ownfoil-{}-conversion.tmp", uuid::Uuid::new_v4())),
    );
    let temporary_path = temporary.0.clone();
    let before = std::fs::metadata(&source)?;
    let settings = management.compression.clone();
    let block = settings.mode == "block" || (settings.mode == "auto" && extension == "xci");
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let keys = nx_archive::formats::Keyset::from_file(keys_path)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        if compress {
            let verdict = verification::verify(&source_copy, &keys, "hash")?;
            ensure!(
                verdict["verificationStatus"] != "CORRUPT",
                "Corrupt files cannot be compressed"
            );
        }
        let (mut source_file, root) = archive::root(&source_copy)?;
        let entries = archive::leaves(&mut source_file, &root)?;
        let title_keys = verification::title_keys(&source_file, &entries)?;
        let mut out =
            OpenOptions::new().read(true).write(true).create_new(true).open(&temporary_path)?;
        out.set_permissions(source_file.metadata()?.permissions())?;
        if root.offset > 0 {
            archive::copy(
                &source_file,
                &archive::Entry { name: String::new(), offset: 0, size: root.offset },
                &mut out,
            )?;
        }
        let header_size =
            archive::rebuild(&mut source_file, &root, &mut out, &mut |source, entry, out| {
                if compress
                    && entry.name.ends_with(".nca")
                    && !entry.name.ends_with(".cnmt.nca")
                    && entry.size > 0x4000
                {
                    ncz::compress(source, entry, out, &keys, &title_keys, &settings, block)?;
                    Ok(format!("{}.ncz", entry.name.trim_end_matches(".nca")))
                } else if !compress && entry.name.ends_with(".ncz") {
                    ncz::decompress(source, entry, out)?;
                    Ok(format!("{}.nca", entry.name.trim_end_matches(".ncz")))
                } else {
                    archive::copy(source, entry, out)?;
                    Ok(entry.name.clone())
                }
            })?;
        if root.offset > 0 {
            let hash = archive::hash(
                &out,
                &archive::Entry { name: String::new(), offset: root.offset, size: header_size },
            )?;
            out.seek(SeekFrom::Start(0x138))?;
            out.write_all(&header_size.to_le_bytes())?;
            out.write_all(&hash)?;
        }
        out.sync_all()?;
        roundtrip(&source_copy, &temporary_path)?;
        Ok(())
    })
    .await??;
    crate::tasks::progress(storage, task_id, 95).await?;
    ensure!(!crate::tasks::cancelled(storage, task_id).await, "Task cancelled");
    let _guard = state.scan_lock.lock().await;
    ensure!(!crate::tasks::cancelled(storage, task_id).await, "Task cancelled");
    let after = std::fs::metadata(&source)?;
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "Source changed during conversion"
    );
    let journal = state.data_dir.join(format!("conversion-{file_id}.json"));
    std::fs::create_dir_all(&state.data_dir)?;
    let data = json!({"source":source,"target":target,"file_id":file_id,"root":root,"library_path":file.library_root});
    let mut record = OpenOptions::new().write(true).create_new(true).open(&journal)?;
    record.write_all(data.to_string().as_bytes())?;
    record.sync_all()?;
    sync_parent(&journal)?;
    // Hard-link publication fails if the target appeared meanwhile; it never overwrites it.
    if let Err(error) = std::fs::hard_link(&temporary.0, &target) {
        std::fs::remove_file(&journal)?;
        return Err(error.into());
    }
    sync_parent(&target)?;
    finalize(state, &data).await?;
    std::fs::remove_file(&journal)?;
    sync_parent(&journal)?;
    Ok(json!({"path":target,"success":true}))
}

async fn save_verification(
    storage: &crate::storage::Storage,
    file_id: i64,
    verdict: Value,
) -> anyhow::Result<()> {
    storage.with_connection(move |conn| {
        // Signature-only checks must not erase the last full hash verdict.
        conn.execute("UPDATE files SET signature_valid=?2,hash_valid=COALESCE(?3,hash_valid),hash_modified=COALESCE(?4,hash_modified),verification_error=?5,verified_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1",rusqlite::params![file_id,verdict["signatureValid"].as_bool(),verdict["hashValid"].as_bool(),verdict["hashModified"].as_bool(),verdict["verificationError"].as_str()])?;
        Ok(())
    }).await?;
    Ok(())
}

fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

async fn finalize(state: &AppState, data: &Value) -> anyhow::Result<()> {
    let storage = state.storage.as_ref().context("Storage unavailable")?;
    let source = PathBuf::from(data["source"].as_str().context("Invalid conversion journal")?);
    let target = PathBuf::from(data["target"].as_str().context("Invalid conversion journal")?);
    let root = PathBuf::from(data["root"].as_str().context("Invalid conversion journal")?);
    ensure!(
        source.starts_with(&root) && target.starts_with(&root),
        "Conversion journal outside root"
    );
    if !target.exists() {
        return Ok(());
    }
    let scan_root = data["library_path"].as_str().map_or_else(|| root.clone(), PathBuf::from);
    ensure!(
        std::fs::canonicalize(&scan_root)? == std::fs::canonicalize(&root)?,
        "Conversion library path changed"
    );
    let relative = target.strip_prefix(&root)?.to_string_lossy().to_string();
    let filename = target.file_name().context("Invalid target name")?.to_string_lossy().to_string();
    let ext = target.extension().context("Invalid target extension")?.to_string_lossy().to_string();
    let compressed = matches!(ext.as_str(), "nsz" | "xcz");
    let metadata = std::fs::metadata(&target)?;
    let size = i64::try_from(metadata.len())?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs_f64());
    let id = data["file_id"].as_i64().context("Invalid file id")?;
    storage
        .with_connection(move |conn| {
            conn.execute(
                "UPDATE files SET path=?2,name=?3,ext=?4,compressed=?5,size=?6,mtime=?7 WHERE id=?1",
                rusqlite::params![id, relative, filename, ext, compressed, size, modified],
            )?;
            Ok(())
        })
        .await?;
    if source.exists() {
        std::fs::remove_file(&source)?;
        sync_parent(&source)?;
    }
    let management = state.settings.read().await.library.management.clone();
    let files = crate::scan_all_libraries(
        std::slice::from_ref(&scan_root),
        storage,
        &management,
        &state.keys_path,
    )
    .await?;
    let mut merged = state
        .catalog
        .read()
        .await
        .files()
        .iter()
        .filter(|f| f.library_root != scan_root)
        .cloned()
        .collect::<Vec<_>>();
    merged.extend(files);
    *state.catalog.write().await = crate::catalog::Catalog::from_files(merged);
    Ok(())
}

pub async fn recover(state: &AppState) -> anyhow::Result<()> {
    if !state.data_dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&state.data_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("conversion-") && name.ends_with(".json") {
            let data: Value = serde_json::from_slice(&std::fs::read(entry.path())?)?;
            let source = Path::new(data["source"].as_str().context("Invalid journal source")?);
            let target = Path::new(data["target"].as_str().context("Invalid journal target")?);
            if source.exists() && target.exists() {
                roundtrip(source, target)?;
            }
            finalize(state, &data).await?;
            std::fs::remove_file(entry.path())?;
            sync_parent(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn signature_checks_preserve_hash_verdict_until_full_recheck() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = crate::storage::Storage::open(dir.path().join("test.db")).await?;
        let id = storage.with_connection(|conn| {
            conn.execute("INSERT INTO libraries(path) VALUES('/games')", [])?;
            conn.execute("INSERT INTO files(library_id,path,folder,name,ext,size) VALUES(1,'test.nsp','','test.nsp','nsp',1)", [])?;
            Ok(conn.last_insert_rowid())
        }).await?;
        save_verification(
            &storage,
            id,
            json!({"signatureValid":true,"hashValid":false,"hashModified":true}),
        )
        .await?;
        save_verification(&storage, id, json!({"signatureValid":false})).await?;
        let read = move |conn: &mut rusqlite::Connection| {
            Ok(conn.query_row(
                "SELECT signature_valid,hash_valid,hash_modified FROM files WHERE id=?1",
                [id],
                |r| Ok((r.get::<_, bool>(0)?, r.get::<_, bool>(1)?, r.get::<_, bool>(2)?)),
            )?)
        };
        assert_eq!(storage.with_connection(read).await?, (false, false, true));
        save_verification(
            &storage,
            id,
            json!({"signatureValid":true,"hashValid":true,"hashModified":false}),
        )
        .await?;
        assert_eq!(storage.with_connection(read).await?, (true, true, false));
        Ok(())
    }
}
