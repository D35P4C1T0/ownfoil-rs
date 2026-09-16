//! Durable background jobs. `SQLite` claims serialize workers; failed jobs survive restarts.
use crate::{http::AppState, storage::Storage};
use anyhow::{Context, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub const NAMES: &[&str] = &[
    "startup",
    "add_file",
    "remove_library",
    "handle_file_added",
    "handle_file_moved",
    "handle_file_deleted",
    "handle_dir_deleted",
    "scan_library",
    "scan_libraries",
    "process_library",
    "process_file",
    "update_titledb",
    "verify_file",
    "compress_file",
    "decompress_file",
    "library_maintenance",
    "remove_outdated_updates",
    "remove_missing_files",
    "add_missing_apps",
    "update_titles",
    "add_missing_apps_for_title",
    "update_titles_for_title",
];

pub async fn enqueue(storage: &Storage, name: &str, input: Value) -> anyhow::Result<Value> {
    if !NAMES.contains(&name) {
        bail!("Unknown task: {name}");
    }
    if !input.is_object() {
        bail!("input must be a JSON object");
    }
    let name = name.to_owned();
    let input = input.to_string();
    let id = storage.with_connection(move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<i64> = tx.query_row(
            "SELECT id FROM tasks WHERE task_name=?1 AND input_json=?2 AND status IN ('pending','running','waiting_for_children')",
            params![name, input], |row| row.get(0)).optional()?;
        let id = if let Some(id) = existing {
            tx.execute("UPDATE tasks SET run_after=NULL WHERE id=?1 AND status='pending'",[id])?;
            if matches!(name.as_str(), "scan_library" | "scan_libraries" | "process_library") {
                tx.execute("UPDATE tasks SET rerun_requested=1 WHERE id=?1 AND status='running'", [id])?;
            }
            id
        } else {
            tx.execute("INSERT INTO tasks(task_name,input_json) VALUES(?1,?2)", params![name,input])?;
            tx.last_insert_rowid()
        };
        tx.commit()?;
        Ok(id)
    }).await?;
    get(storage, id).await?.context("enqueued task disappeared")
}

pub async fn list(storage: &Storage) -> anyhow::Result<Vec<Value>> {
    Ok(storage.with_connection(|conn| {
        let mut stmt = conn.prepare("SELECT id,task_name,status,completion_pct,exit_code,error_message,created_at,started_at,completed_at,run_after,parent_id,worker_id,input_json,output_json FROM tasks ORDER BY id DESC")?;
        let rows = stmt.query_map([], |r| {
            let name: String = r.get(1)?;
            let id: i64 = r.get(0)?;
            Ok(json!({"id": id.to_string(), "taskName":name, "displayName":name.replace('_'," "),
                "status":r.get::<_,String>(2)?.to_ascii_uppercase(), "completionPct":r.get::<_,i64>(3)?,
                "exitCode":r.get::<_,Option<i64>>(4)?, "errorMessage":r.get::<_,Option<String>>(5)?,
                "createdAt":r.get::<_,Option<String>>(6)?, "startedAt":r.get::<_,Option<String>>(7)?,
                "completedAt":r.get::<_,Option<String>>(8)?, "runAfter":r.get::<_,Option<String>>(9)?,
                "parentId":r.get::<_,Option<i64>>(10)?.map(|id|id.to_string()), "workerId":r.get::<_,Option<i64>>(11)?,
                "input":r.get::<_,Option<String>>(12)?, "output":r.get::<_,Option<String>>(13)?}))
        })?;
        rows.collect::<Result<Vec<_>,_>>().map_err(Into::into)
    }).await?)
}

pub async fn get(storage: &Storage, id: i64) -> anyhow::Result<Option<Value>> {
    let id = id.to_string();
    Ok(list(storage).await?.into_iter().find(|task| task["id"] == id))
}

pub async fn dismiss(storage: &Storage, id: Option<i64>) -> anyhow::Result<usize> {
    Ok(storage
        .with_connection(move |conn| {
            Ok(conn.execute(
                "DELETE FROM tasks WHERE status='failed' AND (?1 IS NULL OR id=?1)",
                [id],
            )?)
        })
        .await?)
}

pub async fn cancel(storage: &Storage, id: i64) -> anyhow::Result<bool> {
    // Running I/O is cooperatively cancelled before any source replacement.
    Ok(storage.with_connection(move |conn| Ok(conn.execute(
        "WITH RECURSIVE descendants(id) AS (SELECT id FROM tasks WHERE id=?1 UNION ALL SELECT tasks.id FROM tasks JOIN descendants ON tasks.parent_id=descendants.id) UPDATE tasks SET cancel_requested=1 WHERE id IN (SELECT id FROM descendants) AND status IN ('pending','running','waiting_for_children')", [id])? > 0)).await?)
}

pub async fn cancelled(storage: &Storage, id: i64) -> bool {
    storage
        .with_connection(move |conn| {
            Ok(conn
                .query_row("SELECT cancel_requested FROM tasks WHERE id=?1", [id], |r| {
                    r.get::<_, bool>(0)
                })
                .optional()?
                .unwrap_or(true))
        })
        .await
        .unwrap_or(true)
}

pub async fn progress(storage: &Storage, id: i64, pct: i64) -> anyhow::Result<()> {
    storage
        .with_connection(move |conn| {
            conn.execute(
                "UPDATE tasks SET completion_pct=MAX(completion_pct,?2) WHERE id=?1",
                params![id, pct.clamp(0, 100)],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

pub async fn workers(state: &AppState) -> anyhow::Result<Vec<Value>> {
    let tasks = if let Some(storage) = &state.storage { list(storage).await? } else { Vec::new() };
    Ok((1..=state.settings.read().await.worker.count).map(|id| json!({
        "id":id, "pid":std::process::id(), "alive":true,
        "currentTask":tasks.iter().find(|task| task["workerId"] == id && task["status"] == "RUNNING")
    })).collect())
}

pub async fn start(state: AppState) -> anyhow::Result<()> {
    let storage = state.storage.as_ref().context("task storage unavailable")?;
    storage.with_connection(|conn| {
        conn.execute("UPDATE tasks SET status='failed',exit_code=1,error_message='Interrupted by server restart',completed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE status IN ('running','waiting_for_children')", [])?;
        Ok(())
    }).await?;
    tokio::spawn(async move {
        let mut handles = Vec::new();
        loop {
            let count = state.settings.read().await.worker.count;
            while handles.len() < count {
                let id = handles.len() + 1;
                let state = state.clone();
                handles.push(tokio::spawn(async move {
                    worker(state, id).await;
                }));
            }
            for (index, handle) in handles.iter_mut().enumerate() {
                if handle.is_finished() {
                    let worker_id = index + 1;
                    if let Some(storage) = &state.storage {
                        let _ = storage.with_connection(move |conn| {
                            conn.execute("UPDATE tasks SET status='failed',exit_code=1,error_message='Worker stopped unexpectedly',completed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE worker_id=?1 AND status='running'", [i64::try_from(worker_id).unwrap_or(i64::MAX)])?;
                            Ok(())
                        }).await;
                    }
                    let state = state.clone();
                    *handle = tokio::spawn(async move {
                        worker(state, worker_id).await;
                    });
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    });
    Ok(())
}

async fn worker(state: AppState, worker_id: usize) {
    let Some(storage) = &state.storage else { return };
    loop {
        let settings = state.settings.read().await.clone();
        if worker_id > settings.worker.count {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            continue;
        }
        let io_limit = settings.worker.group_limits.get("io").copied().unwrap_or(1);
        let claim = storage.with_connection(move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute("DELETE FROM tasks WHERE status IN ('pending','waiting_for_children') AND cancel_requested=1", [])?;
            tx.execute("UPDATE tasks SET status=CASE WHEN EXISTS(SELECT 1 FROM tasks AS child WHERE child.parent_id=tasks.id AND child.status='failed') THEN 'failed' ELSE 'completed' END,exit_code=CASE WHEN EXISTS(SELECT 1 FROM tasks AS child WHERE child.parent_id=tasks.id AND child.status='failed') THEN 1 ELSE 0 END,error_message=CASE WHEN EXISTS(SELECT 1 FROM tasks AS child WHERE child.parent_id=tasks.id AND child.status='failed') THEN 'Child task failed' ELSE NULL END,completion_pct=100,completed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'),worker_id=NULL WHERE status='waiting_for_children' AND NOT EXISTS(SELECT 1 FROM tasks AS child WHERE child.parent_id=tasks.id AND child.status IN ('pending','running','waiting_for_children'))", [])?;
            let task: Option<(i64,String,String)> = tx.query_row(
                "SELECT id,task_name,input_json FROM tasks AS candidate WHERE status='pending' AND cancel_requested=0 AND (run_after IS NULL OR run_after<=strftime('%Y-%m-%dT%H:%M:%fZ','now')) AND (task_name NOT IN ('compress_file','decompress_file','verify_file') OR (SELECT COUNT(*) FROM tasks WHERE status='running' AND task_name IN ('compress_file','decompress_file','verify_file')) < ?1) AND NOT EXISTS (SELECT 1 FROM tasks AS active WHERE active.status='running' AND json_extract(candidate.input_json,'$.file_id') IS NOT NULL AND json_extract(active.input_json,'$.file_id')=json_extract(candidate.input_json,'$.file_id')) ORDER BY id LIMIT 1",
                [i64::try_from(io_limit).unwrap_or(i64::MAX)], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some((id,_,_)) = &task {
                tx.execute("UPDATE tasks SET status='running',worker_id=?2,started_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1",params![id,i64::try_from(worker_id).unwrap_or(i64::MAX)])?;
            }
            tx.commit()?;
            Ok(task)
        }).await;
        if let Ok(Some((id, name, input))) = claim {
            let result = match serde_json::from_str(&input) {
                Ok(input) => execute(&state, id, &name, input).await,
                Err(error) => Err(error.into()),
            };
            let (code, error, output) = match result {
                Ok(output) => (0, None, Some(output.to_string())),
                Err(error) => (1, Some(error.to_string()), None),
            };
            if let Err(error) = storage.with_connection(move |conn| {
                conn.execute("DELETE FROM tasks WHERE id=?1 AND cancel_requested=1",[id])?;
                conn.execute("UPDATE tasks SET status=?2,exit_code=?3,error_message=?4,output_json=?5,completion_pct=CASE WHEN ?3=0 THEN 100 ELSE completion_pct END,completed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1 AND status='running'",params![id,if code==0 {"completed"} else {"failed"},code,error,output])?;
                conn.execute("UPDATE tasks SET status='pending',completion_pct=0,exit_code=NULL,error_message=NULL,output_json=NULL,started_at=NULL,completed_at=NULL,worker_id=NULL,rerun_requested=0 WHERE id=?1 AND rerun_requested=1 AND cancel_requested=0", [id])?;
                // Bound successful history; failures remain until explicitly dismissed.
                conn.execute("DELETE FROM tasks WHERE status='completed' AND id NOT IN (SELECT id FROM tasks WHERE status='completed' ORDER BY id DESC LIMIT 200)",[])?;
                Ok(())
            }).await { tracing::error!(%error,"failed to finish task"); }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

async fn execute(state: &AppState, id: i64, name: &str, input: Value) -> anyhow::Result<Value> {
    let storage = state.storage.as_ref().context("task storage unavailable")?;
    if cancelled(storage, id).await {
        bail!("Task cancelled");
    }
    if name == "update_titledb" {
        state.titledb.refresh_and_wait().await?;
    } else if name == "startup" {
        if let Err(error) = state.titledb.refresh_and_wait().await {
            tracing::warn!(%error, "startup metadata refresh failed; scanning local files");
        }
    }
    if matches!(name, "verify_file" | "compress_file" | "decompress_file") {
        return crate::content::run(state, id, name, &input).await;
    }
    if name == "scan_libraries" {
        let roots = state.settings.read().await.library.paths.clone();
        for path in roots {
            let child = enqueue(storage, "scan_library", json!({"library_path":path})).await?;
            let child_id = child["id"].as_str().context("Missing child task id")?.parse::<i64>()?;
            storage
                .with_connection(move |conn| {
                    conn.execute(
                        "UPDATE tasks SET parent_id=?1 WHERE id=?2 AND parent_id IS NULL",
                        params![id, child_id],
                    )?;
                    Ok(())
                })
                .await?;
        }
        storage.with_connection(move |conn| {
            conn.execute("UPDATE tasks SET status='waiting_for_children',worker_id=NULL WHERE id=?1 AND EXISTS(SELECT 1 FROM tasks AS child WHERE child.parent_id=?1)",[id])?;
            Ok(())
        }).await?;
        return Ok(json!({"success":true}));
    }
    let _guard = state.scan_lock.lock().await;
    let settings = state.settings.read().await.clone();
    if lifecycle(state, storage, name, &input).await? {
        return Ok(json!({"success":true}));
    }
    let roots = if let Some(path) = input["library_path"].as_str() {
        let path = std::path::PathBuf::from(path);
        if !settings.library.paths.contains(&path) {
            bail!("Unknown library path");
        }
        vec![path]
    } else {
        settings.library.paths.clone()
    };
    let files =
        crate::scan_all_libraries(&roots, storage, &settings.library.management, &state.keys_path)
            .await?;
    let mut merged = state
        .catalog
        .read()
        .await
        .files()
        .iter()
        .filter(|file| !roots.contains(&file.library_root))
        .cloned()
        .collect::<Vec<_>>();
    merged.extend(files);
    *state.catalog.write().await = crate::catalog::Catalog::from_files(merged);
    queue_pipeline(state).await?;
    progress(storage, id, 100).await?;
    Ok(json!({"success":true}))
}

async fn lifecycle(
    state: &AppState,
    storage: &Storage,
    name: &str,
    input: &Value,
) -> anyhow::Result<bool> {
    if name == "remove_library" {
        let path = input["library_path"].as_str().context("library_path is required")?;
        for library in storage.list_libraries().await? {
            if library.path == path {
                storage.delete_library(library.id).await?;
            }
        }
        let files = state
            .catalog
            .read()
            .await
            .files()
            .iter()
            .filter(|file| file.library_root != std::path::Path::new(path))
            .cloned()
            .collect();
        *state.catalog.write().await = crate::catalog::Catalog::from_files(files);
        storage
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE apps SET owned=EXISTS(SELECT 1 FROM app_files WHERE app_id=apps.id)",
                    [],
                )?;
                Ok(())
            })
            .await?;
        return Ok(true);
    }
    if name == "handle_file_moved" {
        let root = std::path::PathBuf::from(
            input["library_path"].as_str().context("library_path is required")?,
        );
        if !state.settings.read().await.library.paths.contains(&root) {
            bail!("Unknown library path");
        }
        let source =
            std::path::Path::new(input["src_path"].as_str().context("src_path is required")?);
        let target =
            std::path::Path::new(input["dest_path"].as_str().context("dest_path is required")?);
        let source_relative = source.strip_prefix(&root)?.to_string_lossy().to_string();
        let canonical_root = std::fs::canonicalize(&root)?;
        if !std::fs::canonicalize(target)?.starts_with(canonical_root) {
            bail!("File outside library root");
        }
        let relative = target.strip_prefix(&root)?.to_string_lossy().to_string();
        let filename =
            target.file_name().context("Missing filename")?.to_string_lossy().to_string();
        let folder = target.parent().context("Missing parent")?.to_string_lossy().to_string();
        let root = root.to_string_lossy().to_string();
        storage.with_connection(move |conn| {
            conn.execute("UPDATE files SET path=?3,name=?4,folder=?5 WHERE path=?2 AND library_id=(SELECT id FROM libraries WHERE path=?1)", params![root,source_relative,relative,filename,folder])?;
            Ok(())
        }).await?;
    }
    // File additions/deletions converge through the normal root reconciliation.
    Ok(false)
}

async fn sync_known_apps(state: &AppState, storage: &Storage) -> anyhow::Result<()> {
    let titles = storage
        .with_connection(|conn| {
            let mut statement = conn.prepare("SELECT id,title_id FROM titles")?;
            let rows = statement
                .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    let mut known = Vec::new();
    for (row_id, title_id) in titles {
        if title_id.len() != 16 || !title_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        known.push((row_id, title_id.clone(), 0u64, "BASE"));
        if let Some(versions) = state.titledb.versions(&title_id).await {
            let update_id = format!("{}800", &title_id[..13]);
            for version in versions.versions {
                known.push((row_id, update_id.clone(), version, "UPDATE"));
            }
        }
        for dlc in state.titledb.dlc_for_title(&title_id).await {
            known.push((row_id, dlc.title_id, dlc.version.unwrap_or(0), "DLC"));
        }
    }
    storage.with_connection(move |conn| {
        let tx = conn.transaction()?;
        for (title, id, version, kind) in known {
            tx.execute("INSERT INTO apps(title_id,app_id,app_version,app_type,owned) VALUES(?1,?2,?3,?4,0) ON CONFLICT(app_id,app_version) DO NOTHING",params![title,id,version.to_string(),kind])?;
        }
        tx.commit()?;
        Ok(())
    }).await?;
    Ok(())
}

pub async fn queue_pipeline(state: &AppState) -> anyhow::Result<()> {
    let Some(storage) = &state.storage else { return Ok(()) };
    sync_known_apps(state, storage).await?;
    if crate::keys::inspect(&state.keys_path).valid_keys != Some(true) {
        return Ok(());
    }
    let management = state.settings.read().await.library.management.clone();
    let depth = management.verification.depth.clone();
    let pending=storage.with_connection(move |conn| {
        let mut stmt=conn.prepare("SELECT id,compressed,signature_valid,hash_valid,hash_modified,identification_type FROM files")?;
        let rows=stmt.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,bool>(1)?,r.get::<_,Option<bool>>(2)?,r.get::<_,Option<bool>>(3)?,r.get::<_,Option<bool>>(4)?,r.get::<_,Option<String>>(5)?)))?;
        Ok(rows.collect::<Result<Vec<_>,_>>()?)
    }).await?;
    for (id, compressed, signature, hash, modified, identification) in pending {
        if identification.as_deref() != Some("cnmt") {
            continue;
        }
        if management.verification.enabled
            && (signature.is_none() || (depth == "hash" && (hash.is_none() || modified.is_none())))
        {
            enqueue(storage, "verify_file", json!({"file_id":id})).await?;
        } else if management.compression.enabled
            && !compressed
            && crate::content::verification::status(signature, hash, modified) != "CORRUPT"
        {
            enqueue(storage, "compress_file", json!({"file_id":id})).await?;
        }
    }
    Ok(())
}

pub async fn schedule_titledb(
    storage: &Storage,
    interval: &str,
    changed: bool,
) -> anyhow::Result<()> {
    let seconds =
        if interval == "0" { None } else { Some(humantime::parse_duration(interval)?.as_secs()) };
    storage.with_connection(move |conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(seconds)=seconds {
            let modifier=format!("+{seconds} seconds");
            if changed {tx.execute("UPDATE tasks SET run_after=strftime('%Y-%m-%dT%H:%M:%fZ','now',?1) WHERE task_name='update_titledb' AND status='pending' AND run_after IS NOT NULL",[&modifier])?;}
            tx.execute("INSERT INTO tasks(task_name,input_json,run_after) SELECT 'update_titledb','{}',strftime('%Y-%m-%dT%H:%M:%fZ','now',CASE WHEN (SELECT status FROM tasks WHERE task_name='update_titledb' ORDER BY id DESC LIMIT 1)='failed' THEN '+3600 seconds' ELSE ?1 END) WHERE NOT EXISTS(SELECT 1 FROM tasks WHERE task_name='update_titledb' AND status IN ('pending','running'))",[modifier])?;
        } else {tx.execute("DELETE FROM tasks WHERE task_name='update_titledb' AND status='pending' AND run_after IS NOT NULL",[])?;}
        tx.commit()?;Ok(())
    }).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn failed_titledb_refresh_retries_and_manual_enqueue_promotes_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = Storage::open(dir.path().join("tasks.db")).await?;
        storage.with_connection(|conn| {
            conn.execute("INSERT INTO tasks(task_name,input_json,status) VALUES('update_titledb','{}','failed')", [])?;
            Ok(())
        }).await?;
        schedule_titledb(&storage, "12h", false).await?;
        schedule_titledb(&storage, "12h", false).await?;
        storage.with_connection(|conn| {
            let (count, seconds): (i64,i64) = conn.query_row("SELECT COUNT(*),CAST(strftime('%s',run_after) AS INTEGER)-CAST(strftime('%s','now') AS INTEGER) FROM tasks WHERE status='pending'", [], |row| Ok((row.get(0)?,row.get(1)?)))?;
            assert_eq!(count,1);
            assert!((3595..=3600).contains(&seconds));
            Ok(())
        }).await?;
        let promoted = enqueue(&storage, "update_titledb", json!({})).await?;
        assert!(promoted["runAfter"].is_null());
        schedule_titledb(&storage, "0", true).await?;
        assert_eq!(
            list(&storage).await?.iter().filter(|task| task["status"] == "PENDING").count(),
            1
        );
        Ok(())
    }
}
