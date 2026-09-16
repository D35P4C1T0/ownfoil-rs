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
        let result = tokio::task::spawn_blocking(move || {
            let keys = nx_archive::formats::Keyset::from_file(keys_path)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            verification::verify(&source_copy, &keys, &depth)
        })
        .await?;
        let value=result.unwrap_or_else(|error|json!({"signatureValid":false,"hashValid":if management.verification.depth=="hash"{Some(false)}else{None},"hashModified":if management.verification.depth=="hash"{Some(false)}else{None},"verificationError":error.to_string()}));
        let _guard = state.scan_lock.lock().await;
        ensure!(!crate::tasks::cancelled(storage, task_id).await, "Task cancelled");
        let after = std::fs::metadata(&source)?;
        ensure!(
            before.len() == after.len() && before.modified()? == after.modified()?,
            "File changed during verification; scan and retry"
        );
        let save = value.clone();
        storage.with_connection(move |conn| {
            conn.execute("UPDATE files SET signature_valid=?2,hash_valid=?3,hash_modified=?4,verification_error=?5,verified_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1",rusqlite::params![file_id,save["signatureValid"].as_bool(),save["hashValid"].as_bool(),save["hashModified"].as_bool(),save["verificationError"].as_str()])?;Ok(())
        }).await?;
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
    let size = i64::try_from(std::fs::metadata(&target)?.len())?;
    let id = data["file_id"].as_i64().context("Invalid file id")?;
    storage
        .with_connection(move |conn| {
            conn.execute(
                "UPDATE files SET path=?2,name=?3,ext=?4,compressed=?5,size=?6 WHERE id=?1",
                rusqlite::params![id, relative, filename, ext, compressed, size],
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
