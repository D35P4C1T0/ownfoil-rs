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
    let mut input = input;
    for (key, value) in task_scope(&input)?.as_object().context("Invalid scope")? {
        input[key] = value.clone();
    }
    let input = input.to_string();
    let id = storage.with_connection(move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<i64> = tx.query_row(
            "SELECT id FROM tasks WHERE task_name=?1 AND json(input_json)=json(?2) AND cancel_requested=0 AND status IN ('pending','running','waiting_for_children') ORDER BY CASE status WHEN 'running' THEN 0 WHEN 'waiting_for_children' THEN 1 ELSE 2 END,id LIMIT 1",
            params![name, input], |row| row.get(0)).optional()?;
        let id = if let Some(id) = existing {
            tx.execute("UPDATE tasks SET run_after=NULL WHERE id=?1 AND status='pending'",[id])?;
            if matches!(name.as_str(), "scan_library" | "scan_libraries" | "process_library") {
                tx.execute("UPDATE tasks SET rerun_requested=1 WHERE id=?1 AND status IN ('running','waiting_for_children')", [id])?;
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

pub async fn child(
    storage: &Storage,
    parent: i64,
    name: &str,
    input: Value,
) -> anyhow::Result<i64> {
    if !NAMES.contains(&name) || !input.is_object() {
        bail!("Invalid child task");
    }
    let name = name.to_owned();
    let mut input = input;
    for (key, value) in task_scope(&input)?.as_object().context("Invalid scope")? {
        input[key] = value.clone();
    }
    let input = input.to_string();
    let id = storage.with_connection(move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND status='running' AND cancel_requested=0)", [parent], |r| r.get(0))?;
        if !active {
            return Ok(None);
        }
        let existing = tx.query_row("SELECT id FROM tasks WHERE parent_id=?1 AND task_name=?2 AND json(input_json)=json(?3) AND cancel_requested=0 AND status IN ('pending','running','waiting_for_children','completed') ORDER BY id LIMIT 1", params![parent,name,input], |r| r.get::<_,i64>(0)).optional()?;
        let id = if let Some(id) = existing {
            id
        } else {
            tx.execute("INSERT INTO tasks(parent_id,task_name,input_json) VALUES(?1,?2,?3)", params![parent,name,input])?;
            tx.last_insert_rowid()
        };
        tx.commit()?;
        Ok(Some(id))
    }).await?;
    id.context("Parent task is no longer running")
}

async fn wait_for_children(storage: &Storage, id: i64) -> anyhow::Result<()> {
    storage.with_connection(move |conn| {
        conn.execute("UPDATE tasks SET status='waiting_for_children',worker_id=NULL WHERE id=?1 AND status='running' AND cancel_requested=0", [id])?;
        Ok(())
    }).await?;
    Ok(())
}

fn enqueue_followup(conn: &rusqlite::Connection, name: &str, input: &str) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO tasks(task_name,input_json) SELECT ?1,?2 WHERE NOT EXISTS(SELECT 1 FROM tasks WHERE task_name=?1 AND json(input_json)=json(?2) AND cancel_requested=0 AND status IN ('pending','running','waiting_for_children'))", params![name,input])?;
    Ok(())
}

fn settle_parents(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute("UPDATE tasks SET completion_pct=COALESCE((SELECT 100*SUM(child.status IN ('completed','failed'))/COUNT(*) FROM tasks AS child WHERE child.parent_id=tasks.id),0) WHERE status='waiting_for_children' AND cancel_requested=0", [])?;
    loop {
        let ready = {
            let mut stmt = conn.prepare("SELECT id,task_name,rerun_requested FROM tasks WHERE status='waiting_for_children' AND cancel_requested=0 AND NOT EXISTS(SELECT 1 FROM tasks AS child WHERE child.parent_id=tasks.id AND child.status IN ('pending','running','waiting_for_children'))")?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if ready.is_empty() {
            break;
        }
        for (id, name, rerun) in ready {
            let failed = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE parent_id=?1 AND status='failed')",
                [id],
                |r| r.get::<_, bool>(0),
            )?;
            conn.execute("UPDATE tasks SET status=CASE WHEN ?2 THEN 'failed' ELSE 'completed' END,exit_code=CASE WHEN ?2 THEN 1 ELSE 0 END,error_message=CASE WHEN ?2 THEN 'Child task failed' ELSE NULL END,completion_pct=100,completed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'),worker_id=NULL WHERE id=?1", params![id, failed])?;
            let input: String =
                conn.query_row("SELECT input_json FROM tasks WHERE id=?1", [id], |r| r.get(0))?;
            match name.as_str() {
                "scan_library" => {
                    conn.execute("UPDATE libraries SET last_scan=unixepoch() WHERE path=json_extract(?1,'$.library_path')", [&input])?;
                    enqueue_followup(conn, "remove_missing_files", &input)?;
                }
                "process_library" => {
                    enqueue_followup(conn, "library_maintenance", &input)?;
                    enqueue_followup(conn, "update_titles", &input)?;
                }
                _ => {}
            }
            if rerun {
                reset_for_rerun(conn, id)?;
            }
        }
    }
    Ok(())
}

// Keep all children of active parents: deduplication and progress depend on them.
fn prune_history(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM tasks WHERE status='completed' AND NOT EXISTS (SELECT 1 FROM tasks AS parent WHERE parent.id=tasks.parent_id AND parent.status IN ('pending','running','waiting_for_children')) AND id NOT IN (SELECT id FROM tasks WHERE status='completed' ORDER BY id DESC LIMIT 200)",
        [],
    )?;
    Ok(())
}

fn reset_for_rerun(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE tasks SET parent_id=NULL WHERE parent_id=?1", [id])?;
    conn.execute("UPDATE tasks SET status='pending',completion_pct=0,exit_code=NULL,error_message=NULL,output_json=NULL,started_at=NULL,completed_at=NULL,worker_id=NULL,rerun_requested=0 WHERE id=?1", [id])?;
    Ok(())
}

fn display_name(name: &str, input: &Value, filename: Option<&str>) -> String {
    let text = |key: &str| input[key].as_str();
    let basename = |path: &str| path.rsplit('/').next().unwrap_or(path).to_string();
    let file_label = || {
        // Explicit paths take precedence, including null for files already removed.
        let path = if input.get("filepath").is_some() { text("filepath") } else { filename };
        path.filter(|path| !path.is_empty()).map_or_else(
            || {
                format!(
                    "file #{}",
                    input.get("file_id").filter(|id| !id.is_null()).map_or_else(
                        || "None".into(),
                        |id| id.as_str().map_or_else(|| id.to_string(), str::to_string)
                    )
                )
            },
            basename,
        )
    };
    let label = match name {
        "startup" => Some("Startup".into()),
        "update_titledb" => Some("Update TitleDB".into()),
        "scan_libraries" => Some("Scan all libraries".into()),
        "scan_library" => text("library_path").map(|path| format!("Scan {path}")),
        "add_file" => text("filepath").map(|path| format!("Add {}", basename(path))),
        "process_file" => Some(format!("Process {}", file_label())),
        "process_library" => Some("Process library files".into()),
        "library_maintenance" => Some(
            text("library_path")
                .filter(|path| !path.is_empty())
                .map_or_else(|| "Library maintenance".into(), |path| format!("Maintain {path}")),
        ),
        "add_missing_apps_for_title" => {
            text("title_id").map(|id| format!("Add missing content for {id}"))
        }
        "update_titles_for_title" => text("title_id").map(|id| format!("Update title {id}")),
        "remove_outdated_updates" => Some("Remove outdated updates".into()),
        "verify_file" => Some(format!("Verify {}", file_label())),
        "compress_file" => Some(format!("Compress {}", file_label())),
        "decompress_file" => Some(format!("Decompress {}", file_label())),
        "add_missing_apps" => Some("Add missing content".into()),
        "remove_missing_files" => Some("Remove missing files".into()),
        "update_titles" => Some("Update titles".into()),
        "remove_library" => text("library_path").map(|path| format!("Remove library {path}")),
        "handle_file_added" => text("filepath").map(|path| format!("New file {}", basename(path))),
        "handle_file_moved" => text("src_path")
            .zip(text("dest_path"))
            .map(|(source, target)| format!("Moved {} to {}", basename(source), basename(target))),
        "handle_file_deleted" => text("filepath").map(|path| format!("Deleted {}", basename(path))),
        "handle_dir_deleted" => {
            text("dirpath").map(|path| format!("Deleted folder {}", basename(path)))
        }
        _ => None,
    };
    label.unwrap_or_else(|| {
        let humanized = name.replace('_', " ").to_lowercase();
        let mut chars = humanized.chars();
        chars.next().map_or_else(String::new, |first| {
            first.to_uppercase().collect::<String>() + chars.as_str()
        })
    })
}

pub async fn list(storage: &Storage) -> anyhow::Result<Vec<Value>> {
    Ok(storage.with_connection(|conn| {
        let mut stmt = conn.prepare("SELECT tasks.id,task_name,status,completion_pct,exit_code,error_message,created_at,started_at,completed_at,run_after,parent_id,worker_id,input_json,output_json,files.name FROM tasks LEFT JOIN files ON files.id=json_extract(CASE WHEN json_valid(input_json) THEN input_json ELSE '{}' END,'$.file_id') ORDER BY tasks.id DESC")?;
        let rows = stmt.query_map([], |r| {
            let name: String = r.get(1)?;
            let id: i64 = r.get(0)?;
            let input: Option<String> = r.get(12)?;
            let parsed: Value = input.as_deref().and_then(|input| serde_json::from_str(input).ok()).unwrap_or(Value::Null);
            let filename: Option<String> = r.get(14)?;
            let display = display_name(&name, &parsed, filename.as_deref());
            Ok(json!({"id": id.to_string(), "taskName":name, "displayName":display,
                "status":r.get::<_,String>(2)?.to_ascii_uppercase(), "completionPct":r.get::<_,i64>(3)?,
                "exitCode":r.get::<_,Option<i64>>(4)?, "errorMessage":r.get::<_,Option<String>>(5)?,
                "createdAt":r.get::<_,Option<String>>(6)?, "startedAt":r.get::<_,Option<String>>(7)?,
                "completedAt":r.get::<_,Option<String>>(8)?, "runAfter":r.get::<_,Option<String>>(9)?,
                "parentId":r.get::<_,Option<i64>>(10)?.map(|id|id.to_string()), "workerId":r.get::<_,Option<i64>>(11)?,
                "input":input, "output":r.get::<_,Option<String>>(13)?}))
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
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let count = tx.execute(
                "DELETE FROM tasks WHERE status='failed' AND (?1 IS NULL OR id=?1)",
                [id],
            )?;
            settle_parents(&tx)?;
            tx.commit()?;
            Ok(count)
        })
        .await?)
}

pub async fn cancel(storage: &Storage, id: i64) -> anyhow::Result<bool> {
    // Running I/O is cooperatively cancelled before any source replacement.
    Ok(storage
        .with_connection(move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let status: Option<String> = tx
                .query_row("SELECT status FROM tasks WHERE id=?1", [id], |r| r.get(0))
                .optional()?;
            let Some(status) = status else { return Ok(false) };
            if !matches!(status.as_str(), "pending" | "running" | "waiting_for_children") {
                return Ok(false);
            }
            let descendants =
                "WITH RECURSIVE descendants(id) AS (SELECT id FROM tasks WHERE parent_id=?1 UNION ALL SELECT tasks.id FROM tasks JOIN descendants ON tasks.parent_id=descendants.id) ";
            tx.execute(
                &format!(
                    "{descendants}UPDATE tasks SET cancel_requested=1 WHERE (id=?1 OR id IN (SELECT id FROM descendants)) AND status IN ('pending','running','waiting_for_children')"
                ),
                [id],
            )?;
            tx.execute(
                &format!(
                    "{descendants}DELETE FROM tasks WHERE id IN (SELECT id FROM descendants) AND status IN ('pending','waiting_for_children')"
                ),
                [id],
            )?;
            tx.execute("UPDATE tasks SET parent_id=NULL WHERE cancel_requested=1 AND status='running'", [])?;
            tx.execute("DELETE FROM tasks WHERE id=?1 AND status!='running'", [id])?;
            settle_parents(&tx)?;
            tx.commit()?;
            Ok(true)
        })
        .await?)
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
            settle_parents(&tx)?;
            let task: Option<(i64,String,String)> = tx.query_row(
                "SELECT id,task_name,input_json FROM tasks AS candidate WHERE status='pending' AND cancel_requested=0 AND (run_after IS NULL OR run_after<=strftime('%Y-%m-%dT%H:%M:%fZ','now')) AND (task_name NOT IN ('compress_file','decompress_file','verify_file') OR (SELECT COUNT(*) FROM tasks WHERE status='running' AND task_name IN ('compress_file','decompress_file','verify_file')) < ?1) AND NOT EXISTS (SELECT 1 FROM tasks AS active WHERE active.status='running' AND json_extract(candidate.input_json,'$.file_id') IS NOT NULL AND json_extract(active.input_json,'$.file_id')=json_extract(candidate.input_json,'$.file_id')) ORDER BY id LIMIT 1",
                [i64::try_from(io_limit).unwrap_or(i64::MAX)], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some((id, _, _)) = &task {
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
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                if tx.execute("DELETE FROM tasks WHERE id=?1 AND cancel_requested=1",[id])? == 0 {
                    tx.execute("UPDATE tasks SET status=?2,exit_code=?3,error_message=?4,output_json=?5,completion_pct=CASE WHEN ?3=0 THEN 100 ELSE completion_pct END,completed_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1 AND status='running'",params![id,if code==0 {"completed"} else {"failed"},code,error,output])?;
                    tx.execute("UPDATE tasks SET status='pending',completion_pct=0,exit_code=NULL,error_message=NULL,output_json=NULL,started_at=NULL,completed_at=NULL,worker_id=NULL,rerun_requested=0 WHERE id=?1 AND status IN ('completed','failed') AND rerun_requested=1 AND cancel_requested=0", [id])?;
                }
                settle_parents(&tx)?;
                // Bound successful history; failures remain until explicitly dismissed.
                prune_history(&tx)?;
                tx.commit()?;
                Ok(())
            }).await { tracing::error!(%error,"failed to finish task"); }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn execute(state: &AppState, id: i64, name: &str, input: Value) -> anyhow::Result<Value> {
    let storage = state.storage.as_ref().context("task storage unavailable")?;
    if cancelled(storage, id).await {
        bail!("Task cancelled");
    }
    let scope = task_scope(&input)?;
    if scope.get("library_path").is_some() && name != "remove_library" {
        task_roots(state, &scope).await?;
    }
    if matches!(name, "verify_file" | "compress_file" | "decompress_file") {
        return crate::content::run(state, id, name, &input).await;
    }
    if matches!(name, "startup" | "update_titledb") {
        if name == "startup" {
            child(storage, id, "process_library", json!({})).await?;
        }
        let refresh = state.titledb.refresh_and_wait().await;
        if name == "update_titledb" {
            refresh?;
            child(storage, id, "process_library", scope.clone()).await?;
        } else if let Err(error) = refresh {
            tracing::warn!(%error, "startup metadata refresh failed; scanning local files");
        }
        child(storage, id, "add_missing_apps", scope).await?;
        if name == "startup" {
            child(storage, id, "scan_libraries", json!({})).await?;
        }
        wait_for_children(storage, id).await?;
        return Ok(json!({"success":true}));
    }
    if name == "scan_libraries" {
        for path in task_roots(state, &scope).await? {
            child(storage, id, "scan_library", json!({"library_path":path})).await?;
        }
        wait_for_children(storage, id).await?;
        return Ok(json!({"success":true}));
    }
    let _guard = state.scan_lock.lock().await;
    if cancelled(storage, id).await {
        bail!("Task cancelled");
    }
    match name {
        "scan_library" => {
            input["library_path"].as_str().context("library_path is required")?;
            let roots = task_roots(state, &scope).await?;
            for root in roots {
                let files = crate::scanner::scan_library(&root).await?;
                let root_text = root.to_string_lossy().into_owned();
                storage
                    .with_connection(move |conn| {
                        conn.execute(
                            "INSERT INTO libraries(path) VALUES(?1) ON CONFLICT DO NOTHING",
                            [root_text],
                        )?;
                        Ok(())
                    })
                    .await?;
                for file in files {
                    let path = root.join(file.relative_path);
                    if file_changed(storage, &root, &path).await? {
                        child(
                            storage,
                            id,
                            "add_file",
                            json!({"library_path":root,"filepath":path}),
                        )
                        .await?;
                    }
                }
            }
            wait_for_children(storage, id).await?;
        }
        "process_library" => {
            let management = state.settings.read().await.library.management.clone();
            let keys = crate::keys::inspect(&state.keys_path).valid_keys == Some(true);
            for file in selected_files(storage, &scope).await? {
                if file.needs_processing(&management, keys) {
                    child(storage, id, "process_file", json!({"file_id":file.id})).await?;
                }
            }
            wait_for_children(storage, id).await?;
        }
        "process_file" => {
            scope["file_id"].as_i64().context("file_id is required")?;
            if let Some(file) = selected_files(storage, &scope).await?.into_iter().next() {
                process_file(state, storage, id, file).await?;
            }
        }
        "add_file" | "handle_file_added" => {
            let root =
                task_roots(state, &scope).await?.into_iter().next().context("Missing library")?;
            input["library_path"].as_str().context("library_path is required")?;
            let path =
                std::path::Path::new(input["filepath"].as_str().context("filepath is required")?);
            checked_relative(&root, path)?;
            if file_changed(storage, &root, path).await? {
                let mut file = read_content_file(&root, path)?;
                let file_id = persist_file(storage, file.clone(), None).await?;
                file.id = usize::try_from(file_id)?;
                publish_file(state, Some(file), file_id).await;
                storage
                    .with_connection(move |conn| {
                        conn.execute(
                            "UPDATE files SET identification_attempts=0 WHERE id=?1",
                            [file_id],
                        )?;
                        Ok(())
                    })
                    .await?;
                child(storage, id, "process_file", json!({"file_id":file_id})).await?;
                wait_for_children(storage, id).await?;
            }
        }
        "add_missing_apps" | "add_missing_apps_for_title" => {
            if name == "add_missing_apps_for_title" {
                scope["title_id"].as_str().context("title_id is required")?;
            }
            sync_known_apps(state, storage, &scope).await?;
            let followup = if scope["title_id"].is_string() {
                "update_titles_for_title"
            } else {
                "update_titles"
            };
            child(storage, id, followup, scope).await?;
            wait_for_children(storage, id).await?;
        }
        "update_titles" | "update_titles_for_title" => {
            if name == "update_titles_for_title" {
                scope["title_id"].as_str().context("title_id is required")?;
            }
            update_title_flags(storage, &scope).await?;
        }
        "remove_missing_files" | "remove_outdated_updates" | "library_maintenance" => {
            maintain(state, storage, id, name, &scope).await?;
        }
        "remove_library" | "handle_file_moved" | "handle_file_deleted" | "handle_dir_deleted" => {
            lifecycle(state, storage, id, name, &input).await?;
        }
        _ => bail!("Unknown task: {name}"),
    }
    *state.titles_cache.write().await = None;
    Ok(json!({"success":true}))
}

fn task_scope(input: &Value) -> anyhow::Result<Value> {
    let mut scope = json!({});
    for key in ["library_path", "title_id"] {
        if let Some(value) = input.get(key) {
            let text = value
                .as_str()
                .filter(|text| !text.is_empty())
                .with_context(|| format!("Invalid {key}"))?;
            scope[key] =
                json!(if key == "title_id" { text.to_ascii_uppercase() } else { text.to_owned() });
        }
    }
    if let Some(value) = input.get("file_id") {
        let id = value
            .as_i64()
            .or_else(|| value.as_str()?.parse().ok())
            .filter(|id| *id > 0)
            .context("Invalid file_id")?;
        scope["file_id"] = json!(id);
    }
    Ok(scope)
}

async fn task_roots(state: &AppState, scope: &Value) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let roots = state.settings.read().await.library.paths.clone();
    if let Some(path) = scope["library_path"].as_str() {
        let path = std::path::PathBuf::from(path);
        if !roots.contains(&path) {
            bail!("Unknown library path");
        }
        Ok(vec![path])
    } else {
        Ok(roots)
    }
}

struct TaskFile {
    id: i64,
    root: std::path::PathBuf,
    path: std::path::PathBuf,
    identification: Option<String>,
    attempts: i64,
    organized: bool,
    signature: Option<bool>,
    hash: Option<bool>,
    modified: Option<bool>,
}

impl TaskFile {
    fn needs_processing(
        &self,
        management: &crate::settings::LibraryManagementSettings,
        keys: bool,
    ) -> bool {
        self.attempts == 0
            || (keys && self.identification.as_deref() != Some("cnmt"))
            || (management.organizer.enabled && !self.organized)
            || self.next_stage(management, keys).is_some()
    }

    fn next_stage(
        &self,
        management: &crate::settings::LibraryManagementSettings,
        keys: bool,
    ) -> Option<&'static str> {
        if !keys || self.identification.as_deref() != Some("cnmt") {
            return None;
        }
        if management.verification.enabled
            && (self.signature.is_none()
                || (management.verification.depth == "hash"
                    && (self.hash.is_none() || self.modified.is_none())))
        {
            return Some("verify_file");
        }
        let extension = self.path.extension()?.to_str()?.to_ascii_lowercase();
        if management.compression.enabled
            && matches!(extension.as_str(), "nsp" | "xci")
            && crate::content::verification::status(self.signature, self.hash, self.modified)
                != "CORRUPT"
            && !self
                .root
                .join(&self.path)
                .with_extension(if extension == "nsp" { "nsz" } else { "xcz" })
                .exists()
        {
            return Some("compress_file");
        }
        None
    }
}

async fn selected_files(storage: &Storage, scope: &Value) -> anyhow::Result<Vec<TaskFile>> {
    let scope = scope.to_string();
    Ok(storage.with_connection(move |conn| {
        let mut stmt = conn.prepare("SELECT f.id,l.path,f.path,f.identification_type,f.identification_attempts,f.organized,f.signature_valid,f.hash_valid,f.hash_modified FROM files f JOIN libraries l ON l.id=f.library_id WHERE (json_extract(?1,'$.library_path') IS NULL OR l.path=json_extract(?1,'$.library_path')) AND (json_extract(?1,'$.file_id') IS NULL OR f.id=json_extract(?1,'$.file_id')) AND (json_extract(?1,'$.title_id') IS NULL OR f.title_id=json_extract(?1,'$.title_id') OR EXISTS(SELECT 1 FROM app_files af JOIN apps a ON a.id=af.app_id JOIN titles t ON t.id=a.title_id WHERE af.file_id=f.id AND t.title_id=json_extract(?1,'$.title_id'))) ORDER BY f.id")?;
        let rows = stmt.query_map([scope], |r| Ok(TaskFile {
            id:r.get(0)?,root:std::path::PathBuf::from(r.get::<_,String>(1)?),path:std::path::PathBuf::from(r.get::<_,String>(2)?),identification:r.get(3)?,attempts:r.get(4)?,organized:r.get(5)?,signature:r.get(6)?,hash:r.get(7)?,modified:r.get(8)?,
        }))?;
        Ok(rows.collect::<Result<Vec<_>,_>>()?)
    }).await?)
}

fn checked_relative(
    root: &std::path::Path,
    path: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let relative = path.strip_prefix(root)?;
    if relative.as_os_str().is_empty()
        || !relative.components().all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        bail!("Invalid library file path");
    }
    if !std::fs::canonicalize(path)?.starts_with(std::fs::canonicalize(root)?) {
        bail!("File outside library root");
    }
    Ok(relative.to_path_buf())
}

fn read_content_file(
    root: &std::path::Path,
    path: &std::path::Path,
) -> anyhow::Result<crate::catalog::ContentFile> {
    let relative_path = checked_relative(root, path)?;
    if !crate::scanner::is_supported_content(path) || !path.is_file() {
        bail!("Unsupported content file");
    }
    let metadata = std::fs::metadata(path)?;
    let name = path.file_name().context("Missing filename")?.to_string_lossy().into_owned();
    let parsed = crate::catalog::parse_filename_metadata(&name);
    let fallback = crate::catalog::parse_filename_metadata(&relative_path.to_string_lossy());
    let title_id = crate::catalog::to_display_title_id(parsed.title_id.or(fallback.title_id));
    let kind = crate::catalog::classify_title_id(title_id.as_deref());
    Ok(crate::catalog::ContentFile {
        id: 0,
        library_root: root.to_path_buf(),
        relative_path,
        name,
        size: metadata.len(),
        title_id,
        version: parsed.version.or(fallback.version),
        kind,
        identified_contents: Vec::new(),
    })
}

async fn file_changed(
    storage: &Storage,
    root: &std::path::Path,
    path: &std::path::Path,
) -> anyhow::Result<bool> {
    let relative = checked_relative(root, path)?.to_string_lossy().into_owned();
    let root = root.to_string_lossy().into_owned();
    let metadata = std::fs::metadata(path)?;
    let size = i64::try_from(metadata.len())?;
    let mtime = metadata.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs_f64();
    Ok(storage.with_connection(move |conn| {
        Ok(!conn.query_row("SELECT EXISTS(SELECT 1 FROM files f JOIN libraries l ON l.id=f.library_id WHERE l.path=?1 AND f.path=?2 AND f.size=?3 AND f.mtime=?4)", params![root,relative,size,mtime], |r| r.get::<_,bool>(0))?)
    }).await?)
}

async fn persist_file(
    storage: &Storage,
    file: crate::catalog::ContentFile,
    existing: Option<i64>,
) -> anyhow::Result<i64> {
    let mtime = std::fs::metadata(file.library_root.join(&file.relative_path))?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    let id = storage.with_connection(move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let root = file.library_root.to_string_lossy();
        let path = file.relative_path.to_string_lossy();
        let folder = file.relative_path.parent().unwrap_or_else(|| std::path::Path::new("")).to_string_lossy();
        let ext = file.relative_path.extension().and_then(|ext| ext.to_str()).unwrap_or_default().to_ascii_lowercase();
        tx.execute("INSERT INTO libraries(path) VALUES(?1) ON CONFLICT DO NOTHING", [&root])?;
        let library: i64 = tx.query_row("SELECT id FROM libraries WHERE path=?1", [&root], |r|r.get(0))?;
        let kind = |kind| match kind { crate::catalog::ContentKind::Base => "BASE", crate::catalog::ContentKind::Update => "UPDATE", crate::catalog::ContentKind::Dlc => "DLC", crate::catalog::ContentKind::Unknown => "UNKNOWN" };
        let identities = if file.identified_contents.is_empty() {
            file.title_id.as_ref().map(|app_id| vec![crate::catalog::IdentifiedContent { title_id:crate::storage::base_title_id(app_id,file.kind),app_id:app_id.clone(),version:file.version.unwrap_or(0),kind:file.kind }]).unwrap_or_default()
        } else { file.identified_contents.clone() };
        for identity in &identities {
            tx.execute("INSERT INTO titles(title_id) VALUES(?1) ON CONFLICT DO NOTHING", [&identity.title_id])?;
        }
        if let Some(id) = existing {
            tx.execute("UPDATE files SET path=?2 WHERE id=?1", params![id,path])?;
        }
        tx.execute("INSERT INTO files(library_id,path,folder,name,ext,size,compressed) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(library_id,path) DO NOTHING", params![library,path,folder,file.name,ext,i64::try_from(file.size).unwrap_or(i64::MAX),matches!(ext.as_str(),"nsz"|"xcz")])?;
        let id: i64 = tx.query_row("SELECT id FROM files WHERE library_id=?1 AND path=?2",params![library,path],|r|r.get(0))?;
        tx.execute("UPDATE files SET folder=?2,name=?3,ext=?4,size=?5,compressed=?6,title_id=?7,identification_status=?8,identification_type=?9,identification_attempts=identification_attempts+1,identification_error=?10,last_attempt=unixepoch(),nb_content=?11,multicontent=?12,signature_valid=CASE WHEN mtime IS NOT ?13 OR size!=?5 THEN NULL ELSE signature_valid END,hash_valid=CASE WHEN mtime IS NOT ?13 OR size!=?5 THEN NULL ELSE hash_valid END,hash_modified=CASE WHEN mtime IS NOT ?13 OR size!=?5 THEN NULL ELSE hash_modified END,verification_error=CASE WHEN mtime IS NOT ?13 OR size!=?5 THEN NULL ELSE verification_error END,verified_at=CASE WHEN mtime IS NOT ?13 OR size!=?5 THEN NULL ELSE verified_at END,organized=CASE WHEN mtime IS NOT ?13 OR size!=?5 THEN 0 ELSE organized END,mtime=?13,added_at=COALESCE(added_at,strftime('%Y-%m-%dT%H:%M:%fZ','now')) WHERE id=?1",params![id,folder,file.name,ext,i64::try_from(file.size).unwrap_or(i64::MAX),matches!(ext.as_str(),"nsz"|"xcz"),identities.first().map(|identity|&identity.title_id),if identities.is_empty(){"unknown"}else{"identified"},if file.identified_contents.is_empty(){"filename"}else{"cnmt"},identities.is_empty().then_some("Could not determine App ID"),i64::try_from(identities.len()).unwrap_or(i64::MAX),identities.len()>1,mtime])?;
        tx.execute("DELETE FROM app_files WHERE file_id=?1",[id])?;
        for identity in identities {
            tx.execute("INSERT INTO apps(title_id,app_id,app_version,app_type,owned) VALUES((SELECT id FROM titles WHERE title_id=?1),?2,?3,?4,1) ON CONFLICT(app_id,app_version) DO UPDATE SET owned=1",params![identity.title_id,identity.app_id,identity.version.to_string(),kind(identity.kind)])?;
            tx.execute("INSERT OR IGNORE INTO app_files(app_id,file_id) SELECT id,?1 FROM apps WHERE app_id=?2 AND app_version=?3",params![id,identity.app_id,identity.version.to_string()])?;
        }
        tx.execute("UPDATE apps SET owned=EXISTS(SELECT 1 FROM app_files WHERE app_id=apps.id)",[])?;
        tx.commit()?;
        Ok(id)
    }).await?;
    Ok(id)
}

async fn publish_file(state: &AppState, file: Option<crate::catalog::ContentFile>, id: i64) {
    let mut catalog = state.catalog.write().await;
    let mut files = catalog
        .files()
        .iter()
        .filter(|file| i64::try_from(file.id).ok() != Some(id))
        .cloned()
        .collect::<Vec<_>>();
    files.extend(file);
    *catalog = crate::catalog::Catalog::from_files(files);
}

async fn process_file(
    state: &AppState,
    storage: &Storage,
    task: i64,
    pending: TaskFile,
) -> anyhow::Result<()> {
    let path = pending.root.join(&pending.path);
    if !path.try_exists()? {
        storage.delete_file(pending.id).await?;
        publish_file(state, None, pending.id).await;
        update_title_flags(storage, &json!({})).await?;
        return Ok(());
    }
    let mut file = read_content_file(&pending.root, &path)?;
    if pending.identification.as_deref() == Some("cnmt")
        && !file_changed(storage, &pending.root, &path).await?
    {
        let file_id = pending.id;
        file.identified_contents = storage.with_connection(move |conn| {
            let mut stmt = conn.prepare("SELECT t.title_id,a.app_id,CAST(a.app_version AS INTEGER),a.app_type FROM apps a JOIN titles t ON t.id=a.title_id JOIN app_files af ON af.app_id=a.id WHERE af.file_id=?1 ORDER BY a.id")?;
            let rows = stmt.query_map([file_id], |r| {
                let kind: String = r.get(3)?;
                Ok(crate::catalog::IdentifiedContent { title_id:r.get(0)?,app_id:r.get(1)?,version:r.get(2)?,kind:match kind.as_str() { "BASE" => crate::catalog::ContentKind::Base,"UPDATE" => crate::catalog::ContentKind::Update,"DLC" => crate::catalog::ContentKind::Dlc,_ => crate::catalog::ContentKind::Unknown } })
            })?.collect::<Result<Vec<_>,_>>()?;
            Ok(rows)
        }).await?;
        if let Some(primary) = file.identified_contents.first() {
            file.title_id = Some(primary.app_id.clone());
            file.version = Some(primary.version);
            file.kind = primary.kind;
        }
    }
    let mut file = crate::identifier::identify_files(&pending.root, vec![file], &state.keys_path)
        .await
        .into_iter()
        .next()
        .context("Missing identified file")?;
    if cancelled(storage, task).await {
        bail!("Task cancelled");
    }
    file.id = usize::try_from(persist_file(storage, file.clone(), Some(pending.id)).await?)?;
    let mut management = state.settings.read().await.library.management.clone();
    management.delete_older_updates = false;
    management.organizer.remove_empty_folders = false;
    if management.organizer.enabled {
        if let Some(target) =
            crate::organizer::preview(std::slice::from_ref(&file), &management).into_iter().next()
        {
            let destination = pending.root.join(&target.destination);
            if pending.root.join(".ownfoil-organizer-journal.json").symlink_metadata().is_ok() {
                bail!("Organizer journal requires recovery before processing files");
            }
            let parent = destination.parent().context("Missing destination parent")?;
            let ancestor = parent
                .ancestors()
                .find(|path| path.symlink_metadata().is_ok())
                .context("Missing destination ancestor")?;
            if !std::fs::canonicalize(ancestor)?.starts_with(std::fs::canonicalize(&pending.root)?)
            {
                bail!("Destination outside library root");
            }
            if cancelled(storage, task).await {
                bail!("Task cancelled");
            }
            std::fs::create_dir_all(parent)?;
            std::fs::hard_link(&path, &destination)?;
            file.relative_path = target.destination;
            file.name =
                destination.file_name().context("Missing filename")?.to_string_lossy().into_owned();
            if let Err(error) = persist_file(storage, file.clone(), Some(pending.id)).await {
                std::fs::remove_file(&destination)?;
                return Err(error);
            }
            std::fs::remove_file(&path)?;
        }
        let file_id = pending.id;
        storage
            .with_connection(move |conn| {
                conn.execute("UPDATE files SET organized=1 WHERE id=?1", [file_id])?;
                Ok(())
            })
            .await?;
    }
    publish_file(state, Some(file), pending.id).await;
    let scope = json!({"file_id":pending.id});
    sync_known_apps(state, storage, &scope).await?;
    update_title_flags(storage, &scope).await?;
    let current = selected_files(storage, &scope)
        .await?
        .into_iter()
        .next()
        .context("Processed file disappeared")?;
    if let Some(stage) = current
        .next_stage(&management, crate::keys::inspect(&state.keys_path).valid_keys == Some(true))
    {
        child(storage, task, stage, scope).await?;
        wait_for_children(storage, task).await?;
    }
    Ok(())
}

async fn lifecycle(
    state: &AppState,
    storage: &Storage,
    id: i64,
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
        update_title_flags(storage, &json!({})).await?;
        return Ok(true);
    }
    if matches!(name, "handle_file_deleted" | "handle_dir_deleted") {
        let key = if name == "handle_file_deleted" { "filepath" } else { "dirpath" };
        let path = std::path::Path::new(
            input[key].as_str().with_context(|| format!("{key} is required"))?,
        );
        for file in selected_files(storage, &task_scope(input)?).await? {
            let full = file.root.join(&file.path);
            if full == path || (name == "handle_dir_deleted" && full.starts_with(path)) {
                if cancelled(storage, id).await {
                    bail!("Task cancelled");
                }
                storage.delete_file(file.id).await?;
                publish_file(state, None, file.id).await;
            }
        }
        update_title_flags(storage, &json!({})).await?;
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
        let relative = checked_relative(&root, target)?.to_string_lossy().to_string();
        let library = storage.get_library_by_path(root.to_string_lossy().into_owned()).await?;
        let existing = if let Some(library) = library {
            storage.get_file_by_path(library.id, source_relative.clone()).await?
        } else {
            None
        };
        let Some(existing) = existing else {
            child(storage, id, "add_file", json!({"library_path":root,"filepath":target})).await?;
            wait_for_children(storage, id).await?;
            return Ok(true);
        };
        let mut catalog_file = state
            .catalog
            .read()
            .await
            .files()
            .iter()
            .find(|file| i64::try_from(file.id).ok() == Some(existing.id))
            .cloned();
        if let Some(file) = &mut catalog_file {
            file.relative_path = std::path::PathBuf::from(&relative);
            file.name =
                target.file_name().context("Missing filename")?.to_string_lossy().into_owned();
        } else {
            child(storage, id, "process_file", json!({"file_id":existing.id})).await?;
            wait_for_children(storage, id).await?;
        }
        publish_file(state, catalog_file, existing.id).await;
        let filename =
            target.file_name().context("Missing filename")?.to_string_lossy().to_string();
        let folder = target.parent().context("Missing parent")?.to_string_lossy().to_string();
        let root = root.to_string_lossy().to_string();
        storage.with_connection(move |conn| {
            conn.execute("UPDATE files SET path=?3,name=?4,folder=?5 WHERE path=?2 AND library_id=(SELECT id FROM libraries WHERE path=?1)", params![root,source_relative,relative,filename,folder])?;
            Ok(())
        }).await?;
    }
    Ok(true)
}

async fn selected_titles(storage: &Storage, scope: &Value) -> anyhow::Result<Vec<(i64, String)>> {
    let scope = scope.to_string();
    Ok(storage.with_connection(move |conn| {
        let mut stmt = conn.prepare("SELECT t.id,t.title_id FROM titles t WHERE (json_extract(?1,'$.title_id') IS NULL OR t.title_id=json_extract(?1,'$.title_id')) AND ((json_extract(?1,'$.library_path') IS NULL AND json_extract(?1,'$.file_id') IS NULL) OR EXISTS(SELECT 1 FROM apps a JOIN app_files af ON af.app_id=a.id JOIN files f ON f.id=af.file_id JOIN libraries l ON l.id=f.library_id WHERE a.title_id=t.id AND (json_extract(?1,'$.file_id') IS NULL OR f.id=json_extract(?1,'$.file_id')) AND (json_extract(?1,'$.library_path') IS NULL OR l.path=json_extract(?1,'$.library_path')))) ORDER BY t.id")?;
        let rows = stmt.query_map([scope], |r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<Vec<_>,_>>()?;
        Ok(rows)
    }).await?)
}

async fn update_title_flags(storage: &Storage, scope: &Value) -> anyhow::Result<()> {
    let titles = selected_titles(storage, scope).await?;
    storage.with_connection(move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (id, _) in titles {
            tx.execute("UPDATE apps SET owned=EXISTS(SELECT 1 FROM app_files WHERE app_id=apps.id) WHERE title_id=?1",[id])?;
            tx.execute("UPDATE titles SET have_base=EXISTS(SELECT 1 FROM apps WHERE title_id=?1 AND app_type='BASE' AND owned=1),up_to_date=NOT EXISTS(SELECT 1 FROM apps WHERE title_id=?1 AND app_type='UPDATE') OR COALESCE((SELECT MAX(CAST(app_version AS INTEGER)) FROM apps WHERE title_id=?1 AND app_type='UPDATE' AND owned=1),-1)>=(SELECT MAX(CAST(app_version AS INTEGER)) FROM apps WHERE title_id=?1 AND app_type='UPDATE'),complete=NOT EXISTS(SELECT 1 FROM apps a WHERE a.title_id=?1 AND a.app_type='DLC' AND a.owned=0 AND NOT EXISTS(SELECT 1 FROM apps newer WHERE newer.title_id=a.title_id AND newer.app_id=a.app_id AND CAST(newer.app_version AS INTEGER)>CAST(a.app_version AS INTEGER))) WHERE id=?1",[id])?;
        }
        tx.commit()?;
        Ok(())
    }).await?;
    Ok(())
}

async fn maintain(
    state: &AppState,
    storage: &Storage,
    id: i64,
    name: &str,
    scope: &Value,
) -> anyhow::Result<()> {
    let management = state.settings.read().await.library.management.clone();
    if name == "library_maintenance" {
        if management.organizer.enabled
            && management.organizer.remove_empty_folders
            && scope.get("file_id").is_none()
            && scope.get("title_id").is_none()
        {
            for root in task_roots(state, scope).await? {
                for entry in walkdir::WalkDir::new(root).min_depth(1).contents_first(true) {
                    let entry = entry?;
                    if cancelled(storage, id).await {
                        bail!("Task cancelled");
                    }
                    if entry.file_type().is_dir()
                        && std::fs::read_dir(entry.path())?.next().is_none()
                    {
                        std::fs::remove_dir(entry.path())?;
                    }
                }
            }
        }
        if management.delete_older_updates {
            child(storage, id, "remove_outdated_updates", scope.clone()).await?;
            wait_for_children(storage, id).await?;
        }
        return Ok(());
    }
    let titles = selected_titles(storage, scope).await?;
    let mut files = selected_files(storage, scope).await?;
    files.sort_by_key(|file| file.root.join(&file.path).exists());
    for file in files {
        if cancelled(storage, id).await {
            bail!("Task cancelled");
        }
        let path = file.root.join(&file.path);
        let missing = !path.try_exists()?;
        let outdated = if name == "remove_outdated_updates" {
            let file_id = file.id;
            storage.with_connection(move |conn| {
                Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM files f JOIN app_files af ON af.file_id=f.id JOIN apps a ON a.id=af.app_id WHERE f.id=?1 AND f.identification_status='identified' AND f.multicontent=0 AND (SELECT COUNT(*) FROM app_files WHERE file_id=f.id)=1 AND a.app_type='UPDATE' AND EXISTS(SELECT 1 FROM apps newer JOIN app_files nf ON nf.app_id=newer.id JOIN files newer_file ON newer_file.id=nf.file_id WHERE newer.app_id=a.app_id AND CAST(newer.app_version AS INTEGER)>CAST(a.app_version AS INTEGER) AND newer_file.library_id=f.library_id))",[file_id],|r|r.get::<_,bool>(0))?)
            }).await?
        } else {
            false
        };
        let outdated = if outdated && !missing {
            let file_id = file.id;
            let replacements = storage.with_connection(move |conn| {
                let mut stmt = conn.prepare("SELECT l.path,f.path FROM files f JOIN libraries l ON l.id=f.library_id JOIN app_files af ON af.file_id=f.id JOIN apps newer ON newer.id=af.app_id WHERE EXISTS(SELECT 1 FROM app_files old_af JOIN apps old ON old.id=old_af.app_id WHERE old_af.file_id=?1 AND old.app_id=newer.app_id AND CAST(newer.app_version AS INTEGER)>CAST(old.app_version AS INTEGER))")?;
                let rows = stmt.query_map([file_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
                Ok(rows)
            }).await?;
            replacements
                .into_iter()
                .any(|(root, path)| std::path::Path::new(&root).join(path).is_file())
        } else {
            outdated
        };
        if outdated && !missing {
            if cancelled(storage, id).await {
                bail!("Task cancelled");
            }
            checked_relative(&file.root, &path)?;
            std::fs::remove_file(&path)?;
        }
        if missing || outdated {
            storage.delete_file(file.id).await?;
            publish_file(state, None, file.id).await;
        }
    }
    for (_, title) in titles {
        update_title_flags(storage, &json!({"title_id":title})).await?;
    }
    Ok(())
}

async fn sync_known_apps(state: &AppState, storage: &Storage, scope: &Value) -> anyhow::Result<()> {
    let titles = selected_titles(storage, scope).await?;
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
    sync_known_apps(state, storage, &json!({})).await?;
    update_title_flags(storage, &json!({})).await?;
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
            tx.execute("INSERT INTO tasks(task_name,input_json,run_after) SELECT 'update_titledb','{}',strftime('%Y-%m-%dT%H:%M:%fZ','now',CASE WHEN (SELECT status FROM tasks WHERE task_name='update_titledb' ORDER BY id DESC LIMIT 1)='failed' THEN '+3600 seconds' ELSE ?1 END) WHERE NOT EXISTS(SELECT 1 FROM tasks WHERE task_name='update_titledb' AND status IN ('pending','running','waiting_for_children'))",[modifier])?;
        } else {tx.execute("DELETE FROM tasks WHERE task_name='update_titledb' AND status='pending' AND run_after IS NOT NULL",[])?;}
        tx.commit()?;Ok(())
    }).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn task_labels_resolve_files_and_survive_stale_input() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = Storage::open(dir.path().join("labels.db")).await?;
        storage.with_connection(|conn| {
            conn.execute("INSERT INTO libraries(path) VALUES('/games')", [])?;
            conn.execute("INSERT INTO files(id,library_id,path,folder,name,ext,size) VALUES(7,1,'Demo.nsp','','Demo.nsp','nsp',1)", [])?;
            for (name, input) in [
                ("verify_file", r#"{"file_id":7}"#),
                ("compress_file", r#"{"file_id":8}"#),
                ("scan_library", "not json"),
                ("handle_file_moved", r#"{"src_path":"/old/A.nsp","dest_path":"/new/B.nsp"}"#),
                ("verify_file", r#"{"file_id":7,"filepath":"/override/Other.nsp"}"#),
            ] {
                conn.execute("INSERT INTO tasks(task_name,input_json) VALUES(?1,?2)", (name,input))?;
            }
            Ok(())
        }).await?;
        let mut tasks = list(&storage).await?;
        tasks.reverse();
        assert_eq!(
            tasks
                .iter()
                .map(|task| task["displayName"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            [
                "Verify Demo.nsp",
                "Compress file #8",
                "Scan library",
                "Moved A.nsp to B.nsp",
                "Verify Other.nsp"
            ]
        );
        storage
            .with_connection(|conn| {
                conn.execute("DELETE FROM files WHERE id=7", [])?;
                Ok(())
            })
            .await?;
        assert_eq!(
            list(&storage).await?.last().context("Missing task")?["displayName"],
            "Verify file #7"
        );
        assert_eq!(display_name("update_titledb", &Value::Null, None), "Update TitleDB");
        assert_eq!(
            display_name("library_maintenance", &json!({"library_path":"/games"}), None),
            "Maintain /games"
        );
        Ok(())
    }

    async fn test_state() -> anyhow::Result<(tempfile::TempDir, AppState)> {
        use std::sync::Arc;
        use tokio::sync::RwLock;
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("library");
        std::fs::create_dir(&root)?;
        let storage = Storage::open(dir.path().join("tasks.db")).await?;
        let mut settings = crate::settings::Settings::default();
        settings.library.paths = vec![root.clone()];
        settings.library.management.organizer.enabled = false;
        settings.library.management.delete_older_updates = false;
        let (tx, _) = tokio::sync::broadcast::channel(1);
        let state = AppState {
            catalog: Arc::new(RwLock::new(crate::catalog::Catalog::from_files(Vec::new()))),
            library_root: root,
            storage: Some(storage),
            scan_lock: Arc::new(tokio::sync::Mutex::new(())),
            settings: Arc::new(RwLock::new(settings)),
            settings_path: dir.path().join("settings.yaml"),
            keys_path: dir.path().join("keys.txt"),
            auth: Arc::new(crate::auth::AuthSettings::from_users(Vec::new())),
            shop: Arc::new(RwLock::new(crate::shop::ShopConfig::default())),
            insecure_admin_cookie: false,
            sessions: crate::http::SessionStore::new(24),
            titledb: crate::titledb::TitleDb::with_progress(
                crate::config::TitleDbConfig { enabled: false, ..Default::default() },
                dir.path().to_path_buf(),
                None,
            ),
            titles_cache: Arc::new(RwLock::new(None)),
            data_dir: dir.path().to_path_buf(),
            titledb_progress_tx: tx,
        };
        Ok((dir, state))
    }

    async fn run_task(state: &AppState, name: &str, input: Value) -> anyhow::Result<i64> {
        let storage = state.storage.as_ref().context("missing storage")?;
        let task = enqueue(storage, name, input.clone()).await?;
        let id: i64 = task["id"].as_str().context("missing id")?.parse()?;
        run_claimed(state, id, name, input).await?;
        Ok(id)
    }

    async fn run_claimed(
        state: &AppState,
        id: i64,
        name: &str,
        input: Value,
    ) -> anyhow::Result<()> {
        let storage = state.storage.as_ref().context("missing storage")?;
        storage
            .with_connection(move |conn| {
                conn.execute("UPDATE tasks SET status='running' WHERE id=?1", [id])?;
                Ok(())
            })
            .await?;
        execute(state, id, name, input).await?;
        storage
            .with_connection(move |conn| {
                let tx = conn.transaction()?;
                tx.execute(
                    "UPDATE tasks SET status='completed' WHERE id=?1 AND status='running'",
                    [id],
                )?;
                settle_parents(&tx)?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn drain(state: &AppState) -> anyhow::Result<usize> {
        let storage = state.storage.as_ref().context("missing storage")?;
        for count in 0..40 {
            let next = storage.with_connection(|conn| {
                Ok(conn.query_row("SELECT id,task_name,input_json FROM tasks WHERE status='pending' ORDER BY id LIMIT 1",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional()?)
            }).await?;
            let Some((id, name, input)) = next else {
                return Ok(count);
            };
            run_claimed(state, id, &name, serde_json::from_str(&input)?).await?;
        }
        bail!("Task graph did not terminate")
    }

    #[tokio::test]
    async fn scan_graph_discovers_files_once_and_settles_with_scoped_cleanup() -> anyhow::Result<()>
    {
        let (_dir, state) = test_state().await?;
        let root = &state.library_root;
        std::fs::write(root.join("Game [0100000000000000][v0].nsp"), b"dummy")?;
        let storage = state.storage.as_ref().context("missing storage")?;
        let parent = run_task(&state, "scan_libraries", json!({})).await?;
        assert_eq!(
            child_rows(storage, parent).await?,
            vec![("scan_library".into(), "pending".into())]
        );
        assert_eq!(drain(&state).await?, 4);
        assert_eq!(get(storage, parent).await?.context("missing parent")?["status"], "COMPLETED");
        assert_eq!(state.catalog.read().await.files().len(), 1);
        let library = storage
            .get_library_by_path(root.to_string_lossy().into_owned())
            .await?
            .context("missing library")?;
        assert!(library.last_scan.is_some());
        let count =
            list(storage).await?.iter().filter(|task| task["taskName"] == "process_file").count();
        run_task(&state, "scan_library", json!({"library_path":root})).await?;
        assert_eq!(drain(&state).await?, 1);
        assert_eq!(
            list(storage).await?.iter().filter(|task| task["taskName"] == "process_file").count(),
            count
        );
        assert!(
            list(storage)
                .await?
                .iter()
                .filter(|task| task["taskName"] == "remove_missing_files")
                .all(|task| task["input"]
                    .as_str()
                    .is_some_and(|input| input.contains("library_path")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn single_file_jobs_preserve_siblings_and_changed_file_id() -> anyhow::Result<()> {
        let (_dir, state) = test_state().await?;
        let storage = state.storage.as_ref().context("missing storage")?;
        let target = state.library_root.join("Game [0100000000000000][v0].nsp");
        let sibling = state.library_root.join("Sibling [0100000000010000][v0].nsp");
        std::fs::write(&target, b"one")?;
        std::fs::write(&sibling, b"two")?;
        run_task(&state, "add_file", json!({"library_path":state.library_root,"filepath":target}))
            .await?;
        drain(&state).await?;
        assert_eq!(state.catalog.read().await.files().len(), 1);
        let file = selected_files(storage, &json!({})).await?.remove(0);
        let file_id = file.id;
        storage.with_connection(move |conn| { conn.execute("UPDATE files SET download_count=7,signature_valid=1,hash_valid=1,hash_modified=0 WHERE id=?1",[file_id])?; Ok(()) }).await?;
        std::fs::write(&target, b"changed")?;
        run_task(
            &state,
            "handle_file_added",
            json!({"library_path":state.library_root,"filepath":target}),
        )
        .await?;
        drain(&state).await?;
        let stored = storage.get_file(file_id).await?.context("file id changed")?;
        assert_eq!(stored.download_count, 7);
        assert_eq!(stored.size, 7);
        assert!(selected_files(storage, &json!({"file_id":file_id})).await?[0].signature.is_none());
        let before = list(storage).await?.len();
        run_task(&state, "process_file", json!({"file_id":"999999"})).await?;
        assert_eq!(list(storage).await?.len(), before + 1);
        assert!(sibling.exists());
        assert_eq!(state.catalog.read().await.files().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn title_maintenance_is_targeted_and_does_not_scan() -> anyhow::Result<()> {
        let (_dir, state) = test_state().await?;
        let storage = state.storage.as_ref().context("missing storage")?;
        storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO titles(title_id) VALUES('0100000000000000'),('0100000000010000')",
                    [],
                )?;
                Ok(())
            })
            .await?;
        std::fs::remove_dir(&state.library_root)?;
        let parent =
            run_task(&state, "add_missing_apps_for_title", json!({"title_id":"0100000000000000"}))
                .await?;
        assert_eq!(
            child_rows(storage, parent).await?,
            vec![("update_titles_for_title".into(), "pending".into())]
        );
        assert_eq!(drain(&state).await?, 1);
        storage.with_connection(|conn| {
            let rows: (i64,i64) = conn.query_row("SELECT (SELECT COUNT(*) FROM apps),(SELECT up_to_date FROM titles WHERE title_id='0100000000010000')",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
            assert_eq!(rows,(1,0));
            Ok(())
        }).await?;
        run_task(&state, "library_maintenance", json!({})).await?;
        run_task(&state, "remove_missing_files", json!({"file_id":999})).await?;
        assert_eq!(drain(&state).await?, 0);
        assert!(run_task(&state, "update_titles_for_title", json!({})).await.is_err());
        assert!(run_task(&state, "process_file", json!({"file_id":"invalid"})).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn process_library_only_fans_out_pending_target_files() -> anyhow::Result<()> {
        let (_dir, state) = test_state().await?;
        let storage = state.storage.as_ref().context("missing storage")?;
        let path = state.library_root.join("Game [0100000000000000][v0].nsp");
        std::fs::write(&path, b"dummy")?;
        run_task(&state, "add_file", json!({"library_path":state.library_root,"filepath":path}))
            .await?;
        drain(&state).await?;
        let parent =
            run_task(&state, "process_library", json!({"library_path":state.library_root})).await?;
        assert!(child_rows(storage, parent).await?.is_empty());
        assert_eq!(drain(&state).await?, 2);
        let file = selected_files(storage, &json!({})).await?.remove(0);
        let file_id = file.id;
        storage
            .with_connection(move |conn| {
                conn.execute("UPDATE files SET identification_attempts=0 WHERE id=?1", [file_id])?;
                Ok(())
            })
            .await?;
        let parent = run_task(&state, "process_library", json!({"file_id":file_id})).await?;
        assert_eq!(
            child_rows(storage, parent).await?,
            vec![("process_file".into(), "pending".into())]
        );
        assert_eq!(drain(&state).await?, 3);
        Ok(())
    }

    #[tokio::test]
    async fn deleted_directory_uses_path_boundaries_and_move_preserves_id() -> anyhow::Result<()> {
        let (_dir, state) = test_state().await?;
        let storage = state.storage.as_ref().context("missing storage")?;
        for folder in ["games", "games-other"] {
            let directory = state.library_root.join(folder);
            std::fs::create_dir(&directory)?;
            let path = directory.join("Game [0100000000000000][v0].nsp");
            std::fs::write(&path, b"dummy")?;
            run_task(
                &state,
                "add_file",
                json!({"library_path":state.library_root,"filepath":path}),
            )
            .await?;
        }
        drain(&state).await?;
        run_task(&state, "handle_dir_deleted", json!({"dirpath":state.library_root.join("games")}))
            .await?;
        let files = selected_files(storage, &json!({})).await?;
        assert_eq!(files.len(), 1);
        let source = files[0].root.join(&files[0].path);
        let destination = state.library_root.join("renamed.nsp");
        std::fs::rename(&source, &destination)?;
        run_task(
            &state,
            "handle_file_moved",
            json!({"library_path":state.library_root,"src_path":source,"dest_path":destination}),
        )
        .await?;
        assert_eq!(
            storage.get_file(files[0].id).await?.context("lost moved file")?.path,
            "renamed.nsp"
        );
        assert_eq!(
            state.catalog.read().await.files()[0].relative_path,
            std::path::Path::new("renamed.nsp")
        );
        assert_eq!(drain(&state).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn outdated_cleanup_is_targeted_and_keeps_equal_versions_and_bundles()
    -> anyhow::Result<()> {
        let (_dir, state) = test_state().await?;
        let storage = state.storage.as_ref().context("missing storage")?;
        for name in [
            "Old [0100000000000800][v1].nsp",
            "New [0100000000000800][v2].nsp",
            "Copy [0100000000000800][v2].nsp",
            "Bundle [0100000000000800][v1].nsp",
            "Other [0100000000010800][v1].nsp",
        ] {
            let path = state.library_root.join(name);
            std::fs::write(&path, b"dummy")?;
            run_task(
                &state,
                "add_file",
                json!({"library_path":state.library_root,"filepath":path}),
            )
            .await?;
        }
        drain(&state).await?;
        storage
            .with_connection(|conn| {
                conn.execute("UPDATE files SET multicontent=1 WHERE name LIKE 'Bundle%'", [])?;
                Ok(())
            })
            .await?;
        run_task(&state, "remove_outdated_updates", json!({"title_id":"0100000000000000"})).await?;
        assert!(!state.library_root.join("Old [0100000000000800][v1].nsp").exists());
        assert_eq!(selected_files(storage, &json!({})).await?.len(), 4);
        assert_eq!(state.catalog.read().await.files().len(), 4);
        assert_eq!(drain(&state).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn title_flags_use_latest_dlc_and_scoped_missing_file_cleanup() -> anyhow::Result<()> {
        let (_dir, state) = test_state().await?;
        let storage = state.storage.as_ref().context("missing storage")?;
        let path = state.library_root.join("Dlc [0100000000001001][v2].nsp");
        std::fs::write(&path, b"dummy")?;
        run_task(&state, "add_file", json!({"library_path":state.library_root,"filepath":path}))
            .await?;
        drain(&state).await?;
        let file_id = selected_files(storage, &json!({})).await?[0].id;
        storage.with_connection(|conn| {
            conn.execute("INSERT INTO apps(title_id,app_id,app_version,app_type,owned) VALUES((SELECT id FROM titles WHERE title_id='0100000000000000'),'0100000000001001','1','DLC',0)",[])?;
            Ok(())
        }).await?;
        run_task(&state, "update_titles_for_title", json!({"title_id":"0100000000000000"})).await?;
        let complete = storage
            .with_connection(|conn| {
                Ok(conn.query_row(
                    "SELECT complete FROM titles WHERE title_id='0100000000000000'",
                    [],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await?;
        assert!(complete);
        std::fs::remove_file(&path)?;
        run_task(&state, "remove_missing_files", json!({"file_id":file_id})).await?;
        assert!(storage.get_file(file_id).await?.is_none());
        assert!(state.catalog.read().await.files().is_empty());
        let complete = storage
            .with_connection(|conn| {
                Ok(conn.query_row(
                    "SELECT complete FROM titles WHERE title_id='0100000000000000'",
                    [],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await?;
        assert!(!complete);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_while_waiting_for_scan_lock_prevents_work() -> anyhow::Result<()> {
        let (_dir, state) = test_state().await?;
        let storage = state.storage.as_ref().context("missing storage")?;
        let input = json!({"library_path":state.library_root});
        let task = enqueue(storage, "scan_library", input.clone()).await?;
        let id: i64 = task["id"].as_str().context("missing id")?.parse()?;
        storage
            .with_connection(move |conn| {
                conn.execute("UPDATE tasks SET status='running' WHERE id=?1", [id])?;
                Ok(())
            })
            .await?;
        let guard = state.scan_lock.lock().await;
        let running = execute(&state, id, "scan_library", input);
        tokio::pin!(running);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut running).await.is_err()
        );
        cancel(storage, id).await?;
        drop(guard);
        assert!(running.await.is_err());
        assert!(cancelled(storage, id).await);
        assert_eq!(list(storage).await?.len(), 1);
        assert!(storage.list_libraries().await?.is_empty());
        Ok(())
    }

    async fn child_rows(storage: &Storage, parent: i64) -> anyhow::Result<Vec<(String, String)>> {
        let parent = parent.to_string();
        Ok(storage
            .with_connection(move |conn| {
                let mut stmt = conn
                    .prepare("SELECT task_name,status FROM tasks WHERE parent_id=?1 ORDER BY id")?;
                let rows = stmt
                    .query_map([parent], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?)
    }

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

    #[tokio::test]
    async fn cancellation_reaches_descendants_of_running_children() -> anyhow::Result<()> {
        let (_dir, state) = test_state().await?;
        let storage = state.storage.as_ref().context("missing storage")?;
        let parent = storage.with_connection(|conn| {
            conn.execute("INSERT INTO tasks(task_name,status) VALUES('startup','running')", [])?;
            let parent = conn.last_insert_rowid();
            conn.execute("INSERT INTO tasks(task_name,status,parent_id) VALUES('scan_libraries','running',?1)", [parent])?;
            let child = conn.last_insert_rowid();
            conn.execute("INSERT INTO tasks(task_name,status,parent_id) VALUES('scan_library','pending',?1)", [child])?;
            Ok(parent)
        }).await?;
        cancel(storage, parent).await?;
        let tasks = list(storage).await?;
        assert_eq!(tasks.len(), 2);
        for task in tasks {
            assert_eq!(task["status"], "RUNNING");
            assert!(cancelled(storage, task["id"].as_str().context("missing id")?.parse()?).await);
        }
        Ok(())
    }

    #[tokio::test]
    async fn organizer_collision_leaves_source_retryable() -> anyhow::Result<()> {
        let (_dir, state) = test_state().await?;
        let storage = state.storage.as_ref().context("missing storage")?;
        let source = state.library_root.join("Game [0100000000000000][v0].nsp");
        let destination = state.library_root.join("organized.nsp");
        std::fs::write(&source, b"source")?;
        std::fs::write(&destination, b"collision")?;
        let file_id =
            persist_file(storage, read_content_file(&state.library_root, &source)?, None).await?;
        {
            let mut settings = state.settings.write().await;
            settings.library.management.organizer.enabled = true;
            settings.library.management.organizer.templates.base = "organized.nsp".into();
        }
        assert!(run_task(&state, "process_file", json!({"file_id":file_id})).await.is_err());
        assert_eq!(std::fs::read(&source)?, b"source");
        assert_eq!(std::fs::read(&destination)?, b"collision");
        assert!(!selected_files(storage, &json!({"file_id":file_id})).await?[0].organized);
        Ok(())
    }

    #[tokio::test]
    async fn child_dedupes_and_requires_running_parent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = Storage::open(dir.path().join("tasks.db")).await?;
        let parent: i64 = storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO tasks(task_name,status) VALUES('scan_libraries','running')",
                    [],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        let first = child(&storage, parent, "scan_library", json!({"library_path":"/g"})).await?;
        let second = child(&storage, parent, "scan_library", json!({"library_path": "/g"})).await?;
        assert_eq!(first, second);
        assert_eq!(
            child_rows(&storage, parent).await?,
            vec![("scan_library".into(), "pending".into())]
        );
        let reordered =
            child(&storage, parent, "scan_library", json!({"library_path":"/g","unused":1}))
                .await?;
        assert_ne!(first, reordered);
        assert!(child(&storage, 9999, "scan_library", json!({})).await.is_err());
        storage
            .with_connection(move |conn| {
                conn.execute(
                    "UPDATE tasks SET status='waiting_for_children' WHERE id=?1",
                    [parent],
                )?;
                Ok(())
            })
            .await?;
        assert!(child(&storage, parent, "scan_library", json!({})).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn history_pruning_preserves_children_until_parent_settles() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = Storage::open(dir.path().join("tasks.db")).await?;
        storage.with_connection(|conn| {
            conn.execute("INSERT INTO tasks(task_name,status) VALUES('process_library','running')", [])?;
            let parent = conn.last_insert_rowid();
            conn.execute("INSERT INTO tasks(parent_id,task_name,status) VALUES(?1,'process_file','completed')", [parent])?;
            let child = conn.last_insert_rowid();
            for _ in 0..201 {
                conn.execute("INSERT INTO tasks(task_name,status) VALUES('update_titles','completed')", [])?;
            }
            prune_history(conn)?;
            assert!(conn.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)", [child], |r| r.get::<_, bool>(0))?);
            conn.execute("UPDATE tasks SET status='waiting_for_children' WHERE id=?1", [parent])?;
            settle_parents(conn)?;
            prune_history(conn)?;
            assert!(!conn.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)", [child], |r| r.get::<_, bool>(0))?);
            Ok(())
        }).await?;
        Ok(())
    }

    #[tokio::test]
    async fn settlement_fails_parent_with_failed_child_and_enqueues_followups() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let storage = Storage::open(dir.path().join("tasks.db")).await?;
        let parent: i64 = storage
            .with_connection(|conn| {
                conn.execute("INSERT INTO tasks(task_name,status) VALUES('process_library','waiting_for_children')", [])?;
                let parent = conn.last_insert_rowid();
                conn.execute("INSERT INTO tasks(parent_id,task_name,status) VALUES(?1,'process_file','completed')", [parent])?;
                conn.execute("INSERT INTO tasks(parent_id,task_name,status) VALUES(?1,'process_file','failed')", [parent])?;
                Ok(parent)
            })
            .await?;
        storage
            .with_connection(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                settle_parents(&tx)?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        let settled = get(&storage, parent).await?.context("missing parent")?;
        assert_eq!(settled["status"], "FAILED");
        assert_eq!(settled["errorMessage"], "Child task failed");
        assert_eq!(settled["completionPct"], 100);
        let followups = storage
            .with_connection(|conn| {
                let mut stmt = conn
                    .prepare("SELECT task_name FROM tasks WHERE parent_id IS NULL AND task_name IN ('library_maintenance','update_titles') ORDER BY task_name")?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;
        assert_eq!(followups, vec!["library_maintenance", "update_titles"]);
        Ok(())
    }

    #[tokio::test]
    async fn rerun_requested_reset_detaches_children_and_requeues() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = Storage::open(dir.path().join("tasks.db")).await?;
        let parent: i64 = storage
            .with_connection(|conn| {
                conn.execute("INSERT INTO tasks(task_name,status,rerun_requested) VALUES('scan_libraries','waiting_for_children',1)", [])?;
                let parent = conn.last_insert_rowid();
                conn.execute("INSERT INTO tasks(parent_id,task_name,status) VALUES(?1,'scan_library','completed')", [parent])?;
                Ok(parent)
            })
            .await?;
        storage
            .with_connection(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                settle_parents(&tx)?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        let parent = get(&storage, parent).await?.context("missing parent")?;
        assert_eq!(parent["status"], "PENDING");
        assert_eq!(parent["completionPct"], 0);
        let children =
            child_rows(&storage, parent["id"].as_str().context("missing parent id")?.parse()?)
                .await?;
        assert_eq!(children, Vec::<(String, String)>::new());
        Ok(())
    }

    #[tokio::test]
    async fn cancel_of_waiting_parent_deletes_pending_and_orphans_running_children()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let storage = Storage::open(dir.path().join("tasks.db")).await?;
        let (parent, pending_child, running_child) = storage
            .with_connection(|conn| {
                conn.execute("INSERT INTO tasks(task_name,status) VALUES('scan_libraries','waiting_for_children')", [])?;
                let parent = conn.last_insert_rowid();
                conn.execute("INSERT INTO tasks(parent_id,task_name,status) VALUES(?1,'scan_library','pending')", [parent])?;
                let pending = conn.last_insert_rowid();
                conn.execute("INSERT INTO tasks(parent_id,task_name,status,worker_id) VALUES(?1,'scan_library','running',1)", [parent])?;
                Ok((parent, pending, conn.last_insert_rowid()))
            })
            .await?;
        assert!(cancel(&storage, parent).await?);
        assert!(get(&storage, parent).await?.is_none());
        assert!(get(&storage, pending_child).await?.is_none());
        let orphaned = get(&storage, running_child).await?.context("missing running child")?;
        assert_eq!(orphaned["status"], "RUNNING");
        assert!(orphaned["parentId"].is_null());
        Ok(())
    }
}
