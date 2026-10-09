//! Parameterized native app catalog queries with page-scoped relationship hydration.
use super::{AppState, GraphData};
use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::LazyLock};

// A process-local generation fence forces a rebuild after restart, even if the
// persisted provider's generation number happens to equal the new instance's.
static PROVIDERS: LazyLock<tokio::sync::Mutex<BTreeMap<PathBuf, (usize, u64)>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(BTreeMap::new()));

async fn sync_provider(state: &AppState) -> anyhow::Result<()> {
    let storage = state.storage.as_ref().ok_or_else(|| anyhow::anyhow!("Storage unavailable"))?;
    let mut generations = PROVIDERS.lock().await;
    let generation = state.titledb.generation();
    if generations.get(storage.path()) == Some(&(state.titledb.cache_identity(), generation)) {
        return Ok(());
    }
    let records = state.titledb.records().await;
    let mut dates = Vec::new();
    for record in &records {
        if let Some(id) =
            record["titleId"].as_str().filter(|id| id.ends_with("000") && id.len() == 16)
        {
            if let Some(versions) = state.titledb.versions(id).await {
                let update = format!("{}800", &id[..13]);
                for (version, date) in versions.release_dates {
                    dates.push((update.clone(), version.to_string(), date));
                }
            }
        }
    }
    storage.with_connection(move |conn| {
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM graph_provider_metadata", [])?;
        tx.execute("DELETE FROM graph_app_dates", [])?;
        {
            let mut stmt = tx.prepare("INSERT INTO graph_provider_metadata(title_id,record) VALUES (?1,?2)")?;
            for record in records { if let Some(id) = record["titleId"].as_str() { stmt.execute(rusqlite::params![id,record.to_string()])?; } }
            let mut stmt = tx.prepare("INSERT OR REPLACE INTO graph_app_dates(app_id,app_version,release_date) VALUES (?1,?2,?3)")?;
            for (app,version,date) in dates { stmt.execute(rusqlite::params![app,version,date])?; }
        }
        tx.commit()?;
        Ok(())
    }).await?;
    generations.insert(storage.path().to_path_buf(), (state.titledb.cache_identity(), generation));
    drop(generations);
    Ok(())
}
fn bind(values: &mut Vec<SqlValue>, value: &Value) -> String {
    values.push(match value {
        Value::Bool(v) => SqlValue::Integer(i64::from(*v)),
        Value::Number(v) => SqlValue::Integer(v.as_i64().unwrap_or(i64::MAX)),
        _ => SqlValue::Text(value.as_str().unwrap_or_default().to_string()),
    });
    format!("?{}", values.len())
}
fn predicate(column: &str, wanted: &Value, values: &mut Vec<SqlValue>) -> Vec<String> {
    if wanted.is_null() {
        return Vec::new();
    }
    let Some(operators) = wanted.as_object() else {
        return vec![format!("{column}={}", bind(values, wanted))];
    };
    operators
        .iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(op, value)| match op.as_str() {
            "eq" => format!("{column}={}", bind(values, value)),
            "gte" => format!("{column}>={}", bind(values, value)),
            "lte" => format!("{column}<={}", bind(values, value)),
            "contains" => {
                format!("instr(unicode_lower({column}),unicode_lower({}))>0", bind(values, value))
            }
            "in" | "notIn" => {
                let items = value.as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    return "1".to_string();
                }
                let params = items.iter().map(|v| bind(values, v)).collect::<Vec<_>>().join(",");
                format!("{column} {} ({params})", if op == "in" { "IN" } else { "NOT IN" })
            }
            _ => "0".to_string(),
        })
        .collect()
}
impl GraphData {
    #[allow(clippy::too_many_lines)] // Keep SQL selection and its page projection together.
    pub async fn app_page(
        state: &AppState,
        can_admin: bool,
        args: &Value,
    ) -> anyhow::Result<Value> {
        sync_provider(state).await?;
        let storage =
            state.storage.as_ref().ok_or_else(|| anyhow::anyhow!("Storage unavailable"))?;
        let grouped = args["groupByAppId"] == true;
        let mut conditions = Vec::new();
        let mut after = Vec::new();
        let mut values = Vec::new();
        for wanted in
            [args["owned"].as_bool(), args["filter"]["owned"].as_bool()].into_iter().flatten()
        {
            let p = format!("owned={}", bind(&mut values, &json!(wanted)));
            if grouped && !wanted {
                after.push(p);
            } else {
                conditions.push(p);
            }
        }
        for (key, column) in [
            ("titleId", "title_id"),
            ("appId", "app_id"),
            ("appVersion", "version"),
            ("appType", "app_type"),
        ] {
            conditions.extend(predicate(column, &args["filter"][key], &mut values));
        }
        if args["appType"].as_array().is_some_and(|v| !v.is_empty()) {
            conditions.extend(predicate("app_type", &json!({"in":args["appType"]}), &mut values));
        }
        if let Some(wanted) = args["complete"].as_bool() {
            conditions.push(format!(
                "app_type='BASE' AND complete={}",
                bind(&mut values, &json!(wanted))
            ));
        }
        if let Some(wanted) = args["upToDate"].as_bool() {
            conditions.push(format!("up_to_date={}", bind(&mut values, &json!(wanted))));
        }
        if let Some(search) = args["search"].as_str() {
            let param = bind(&mut values, &json!(search));
            conditions.push(format!("(instr(unicode_lower(coalesce(name,'')),unicode_lower({param}))>0 OR instr(unicode_lower(coalesce(app_name,'')),unicode_lower({param}))>0 OR instr(unicode_lower(title_id),unicode_lower({param}))>0 OR instr(unicode_lower(app_id),unicode_lower({param}))>0)"));
        }
        if let Some(id) = args["_id"].as_str() {
            conditions.push(format!("id={}", bind(&mut values, &json!(id))));
        }
        let where_sql = if conditions.is_empty() { "1".into() } else { conditions.join(" AND ") };
        if grouped {
            after.push("rank=1".to_string());
        }
        let after_sql = if after.is_empty() { "1".into() } else { after.join(" AND ") };
        let default = if grouped { "app_id" } else { "id" };
        let order = match args["orderBy"]["field"].as_str().unwrap_or("ID") {
            "NAME" => "unicode_lower(name)",
            "VERSION" => "version",
            "RELEASE_DATE" => "release_date",
            "ADDED_AT" => "added_at",
            _ => default,
        };
        let direction = if args["orderBy"]["direction"] == "DESC" { "DESC" } else { "ASC" };
        // Missing metadata fields fall through individually, matching provider precedence.
        let cte=format!("WITH projected AS (
            SELECT a.id,t.title_id,a.app_id,CAST(a.app_version AS INTEGER) AS version,upper(a.app_type) AS app_type,a.owned,t.complete,
            CASE WHEN upper(a.app_type)='BASE' THEN t.up_to_date ELSE EXISTS(SELECT 1 FROM apps v WHERE v.app_id=a.app_id AND v.owned=1 AND CAST(v.app_version AS INTEGER)=(SELECT MAX(CAST(w.app_version AS INTEGER)) FROM apps w WHERE w.app_id=a.app_id)) END AS up_to_date,
            coalesce(json_extract(c.record,'$.name'),json_extract(p.record,'$.name'),json_extract(e.record,'$.name')) AS name,
            coalesce(json_extract(ac.record,'$.name'),json_extract(ap.record,'$.name'),json_extract(ae.record,'$.name')) AS app_name,
            d.release_date,(SELECT MAX(f.added_at) FROM app_files af JOIN files f ON af.file_id=f.id WHERE af.app_id=a.id) AS added_at
            FROM apps a JOIN titles t ON a.title_id=t.id LEFT JOIN graph_provider_metadata p ON p.title_id=t.title_id LEFT JOIN title_overrides c ON c.title_id=t.title_id LEFT JOIN extracted_title_overrides e ON e.title_id=t.title_id LEFT JOIN graph_provider_metadata ap ON ap.title_id=a.app_id LEFT JOIN title_overrides ac ON ac.title_id=a.app_id LEFT JOIN extracted_title_overrides ae ON ae.title_id=a.app_id LEFT JOIN graph_app_dates d ON d.app_id=a.app_id AND d.app_version=a.app_version
        ), filtered AS (SELECT * FROM projected WHERE {where_sql}), ranked AS (SELECT *,ROW_NUMBER() OVER (PARTITION BY app_id ORDER BY version DESC,id ASC) AS rank,MAX(owned) OVER(PARTITION BY app_id) AS group_owned FROM filtered), selected AS (SELECT * FROM {} WHERE {after_sql})",if grouped {"(SELECT id,title_id,app_id,version,app_type,group_owned AS owned,complete,up_to_date,name,app_name,release_date,added_at,rank FROM ranked)"}else{"filtered"});
        let size = args["pageSize"].as_i64().unwrap_or(100).clamp(1, 1000);
        let page = args["page"].as_i64().unwrap_or(1).max(1);
        let offset = page.saturating_sub(1).saturating_mul(size);
        let rows=storage.with_connection(move |conn|{
            conn.create_scalar_function("unicode_lower",1,rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC | rusqlite::functions::FunctionFlags::SQLITE_UTF8,|ctx| {
                Ok(ctx.get::<Option<String>>(0)?.map(|text|text.to_lowercase()))
            })?;
            let total:i64=conn.query_row(&format!("{cte} SELECT COUNT(*) FROM selected"),rusqlite::params_from_iter(&values),|r|r.get(0))?;
            let sql=format!("{cte} SELECT id,title_id,owned FROM selected ORDER BY ({order} IS NULL),{order} {direction},{default} ASC LIMIT {size} OFFSET {offset}");
            let mut stmt=conn.prepare(&sql)?;
            let rows=stmt.query_map(rusqlite::params_from_iter(&values),|r|Ok((r.get::<_,i64>(0)?.to_string(),r.get::<_,String>(1)?,r.get::<_,bool>(2)?)))?.collect::<Result<Vec<_>,_>>()?;
            Ok((total,rows))
        }).await?;
        let ids = rows
            .1
            .iter()
            .map(|(_, title, _)| title.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if args["_wantItems"] == false {
            return Ok(json!({"total":rows.0,"items":[]}));
        }
        let want_files = (can_admin && args["_wantFiles"].as_bool().unwrap_or(true))
            || args["_wantDownloads"].as_bool().unwrap_or(true);
        let data = Self::load_scoped(
            state,
            can_admin,
            true,
            Some(ids),
            want_files,
            args["_wantMedia"].as_bool().unwrap_or(true),
        )
        .await?;
        let items=rows.1.into_iter().filter_map(|(id,_,owned)| {
            let mut app=data.apps.iter().find(|a|a["id"]==id)?.clone();
            app["owned"]=json!(owned);
            app["_title"]=data.title(&app["titleId"]); app["_titledb"]=data.title(&app["appId"]);
            let version_id=if app["appType"]=="BASE" {format!("{}800",app["titleId"].as_str().unwrap_or_default().get(..13).unwrap_or_default())}else{app["appId"].as_str().unwrap_or_default().to_string()};
            app["_versions"]=json!(data.apps.iter().filter(|a|a["appId"]==version_id).map(|a|json!({"version":a["appVersion"],"owned":a["owned"],"releaseDate":a["releaseDate"],"displayVersion":a["displayVersion"]})).collect::<Vec<_>>());
            app["_files"]=json!(data.files.iter().filter(|f|app["_fileIds"].as_array().is_some_and(|ids|ids.contains(&f["id"]))).cloned().map(|mut file| { file["_apps"]=json!(data.apps.iter().filter(|a|a["_fileIds"].as_array().is_some_and(|ids|ids.contains(&file["id"]))).cloned().collect::<Vec<_>>()); file }).collect::<Vec<_>>());
            Some(app)
        }).collect::<Vec<_>>();
        let page = json!({"total":rows.0,"items":items});
        #[cfg(test)]
        let page = {
            let mut page = page;
            page["_hydratedCounts"] =
                json!({"titles":data.titles.len(),"apps":data.apps.len(),"files":data.files.len()});
            page
        };
        Ok(page)
    }
}
