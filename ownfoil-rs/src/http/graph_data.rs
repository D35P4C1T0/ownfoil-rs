//! GraphQL data projection and common filter semantics.
use super::AppState;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default)]
pub struct GraphData {
    pub titles: Vec<Value>,
    pub apps: Vec<Value>,
    pub files: Vec<Value>,
    pub libraries: Vec<Value>,
    pub tasks: Vec<Value>,
    pub workers: Vec<Value>,
    pub can_admin: bool,
}
impl GraphData {
    #[allow(clippy::too_many_lines)] // One snapshot keeps related resolver data consistent.
    pub async fn load(state: &AppState, can_admin: bool) -> anyhow::Result<Self> {
        let mut data = Self { can_admin, ..Self::default() };
        data.titles = state.titledb.records().await;
        let files = state.catalog.read().await.files().to_vec();
        let mut links = BTreeMap::<String, Vec<String>>::new();
        if let Some(storage) = &state.storage {
            let (apps,stored_files,links_found,overrides)=storage.with_connection(|conn| {
                fn query(conn:&rusqlite::Connection,sql:&str)->Result<Vec<Value>,crate::storage::StorageError> {
                    let mut stmt=conn.prepare(sql)?;
                    let names=stmt.column_names().iter().map(|s|(*s).to_string()).collect::<Vec<_>>();
                    let rows=stmt.query_map([],|r| {
                        let mut value=serde_json::Map::new();
                        for (i,name) in names.iter().enumerate() {
                            let item=match r.get_ref(i)? {

                                rusqlite::types::ValueRef::Integer(n)=>json!(n),
                                rusqlite::types::ValueRef::Real(n)=>json!(n),
                                rusqlite::types::ValueRef::Text(s)=>json!(String::from_utf8_lossy(s)),
                                rusqlite::types::ValueRef::Null | rusqlite::types::ValueRef::Blob(_)=>Value::Null,
                            };value.insert(name.clone(),item);
                        }Ok(Value::Object(value))
                    })?;Ok(rows.collect::<Result<Vec<_>,_>>()?)
                }
                Ok((query(conn,"SELECT a.id,t.title_id AS titleId,a.app_id AS appId,CAST(a.app_version AS INTEGER) AS appVersion,a.app_type AS appType,a.owned FROM apps a JOIN titles t ON a.title_id=t.id")?,
                    query(conn,"SELECT id,library_id AS libraryId,download_count AS downloadCount,identification_type AS identificationType,identification_error AS identificationError,identification_attempts AS identificationAttempts,nb_content AS nbContent,organized,signature_valid AS signatureValid,hash_valid AS hashValid,hash_modified AS hashModified,verification_error AS verificationError,verified_at AS verifiedAt,mtime,added_at AS addedAt FROM files")?,
                    query(conn,"SELECT app_id,file_id FROM app_files")?,query(conn,"SELECT title_id,record,'extract' AS source FROM extracted_title_overrides UNION ALL SELECT title_id,record,'custom' AS source FROM title_overrides")?))
            }).await?;
            data.apps = apps;
            for app in &mut data.apps {
                app["id"] = app["id"].to_string().into();
                app["owned"] = json!(app["owned"].as_i64() == Some(1));
                app["appType"] =
                    app["appType"].as_str().unwrap_or("BASE").to_ascii_uppercase().into();
            }
            for link in links_found {
                links
                    .entry(link["app_id"].to_string())
                    .or_default()
                    .push(link["file_id"].to_string());
            }
            for library in storage.list_libraries().await? {
                data.libraries.push(json!({"id":library.id.to_string(),"path":library.path,"lastScan":library.last_scan.map(|n|n.to_string())}));
            }
            for file in &files {
                let mut value = file_value(file, &data.libraries);
                if let Some(stored) = stored_files.iter().find(|stored| {
                    stored["id"].as_i64() == Some(i64::try_from(file.id).unwrap_or(i64::MAX))
                }) {
                    for (key, item) in stored.as_object().into_iter().flatten() {
                        if key == "id" {
                            continue;
                        }
                        let item = if matches!(
                            key.as_str(),
                            "organized" | "signatureValid" | "hashValid" | "hashModified"
                        ) && !item.is_null()
                        {
                            json!(item.as_i64() == Some(1))
                        } else {
                            item.clone()
                        };
                        value[key] = item;
                    }
                }
                value["verificationStatus"] = crate::content::verification::status(
                    value["signatureValid"].as_bool(),
                    value["hashValid"].as_bool(),
                    value["hashModified"].as_bool(),
                )
                .into();
                data.files.push(value);
            }
            for row in overrides {
                let id = row["title_id"].as_str().unwrap_or_default();
                if let Ok(Value::Object(record)) =
                    serde_json::from_str::<Value>(row["record"].as_str().unwrap_or("{}"))
                {
                    if let Some(title) = data.titles.iter_mut().find(|t| t["titleId"] == id) {
                        for (key, value) in record {
                            if !value.is_null() {
                                title[key] = value;
                            }
                        }
                        title["source"] = row["source"].clone();
                    } else {
                        let mut title = Value::Object(record);
                        title["titleId"] = id.into();
                        title["source"] = row["source"].clone();
                        data.titles.push(title);
                    }
                }
            }
            data.tasks = crate::tasks::list(storage).await?;
            data.workers = crate::tasks::workers(state).await?;
        } else {
            for file in &files {
                data.files.push(file_value(file, &[]));
                let identities = if file.identified_contents.is_empty() {
                    file.title_id
                        .as_ref()
                        .map(|id| {
                            vec![crate::catalog::IdentifiedContent {
                                title_id: crate::storage::base_title_id(id, file.kind),
                                app_id: id.clone(),
                                version: file.version.unwrap_or(0),
                                kind: file.kind,
                            }]
                        })
                        .unwrap_or_default()
                } else {
                    file.identified_contents.clone()
                };
                for identity in identities {
                    let id = format!("{}:{}", identity.app_id, identity.version);
                    links.entry(id.clone()).or_default().push(file.id.to_string());
                    if data.apps.iter().any(|app| app["id"] == id) {
                        continue;
                    }
                    data.apps.push(json!({"id":id,"titleId":identity.title_id,"appId":identity.app_id,"appVersion":identity.version,"appType":match identity.kind {crate::catalog::ContentKind::Update=>"UPDATE",crate::catalog::ContentKind::Dlc=>"DLC",_=>"BASE"},"owned":true}));
                }
            }
        }
        let tracked = data
            .apps
            .iter()
            .filter_map(|a| a["titleId"].as_str().map(str::to_string))
            .collect::<BTreeSet<_>>();
        for id in tracked {
            if id.len() != 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            if state.storage.is_none() {
                add_known(&mut data.apps, &id, &id, 0, "BASE");
            }
            if !data.titles.iter().any(|t| t["titleId"] == id) {
                data.titles.push(json!({"titleId":id,"source":"titledb","name":id}));
            }
            let update = format!("{}800", &id[..13]);
            if let Some(versions) = state.titledb.versions(&id).await {
                for version in versions.versions {
                    if state.storage.is_none() {
                        add_known(&mut data.apps, &id, &update, version, "UPDATE");
                    }
                }
                for app in data.apps.iter_mut().filter(|a| a["appId"] == update) {
                    if let Some(date) =
                        app["appVersion"].as_u64().and_then(|v| versions.release_dates.get(&v))
                    {
                        app["releaseDate"] = date.clone().into();
                    }
                }
            }
            for dlc in state.titledb.dlc_for_title(&id).await {
                if state.storage.is_none() {
                    add_known(&mut data.apps, &id, &dlc.title_id, dlc.version.unwrap_or(0), "DLC");
                }
            }
        }
        for app in &mut data.apps {
            app["_fileIds"] = json!(
                links.get(app["id"].as_str().unwrap_or_default()).cloned().unwrap_or_default()
            );
        }
        for title in &mut data.titles {
            let id = title["titleId"].as_str().unwrap_or_default();
            let apps = data.apps.iter().filter(|a| a["titleId"] == id).collect::<Vec<_>>();
            let owned = apps.iter().any(|a| a["owned"] == true);
            let base = apps.iter().any(|a| a["owned"] == true && a["appType"] == "BASE");
            let latest = apps
                .iter()
                .filter(|a| a["appType"] == "UPDATE")
                .filter_map(|a| a["appVersion"].as_u64())
                .max();
            let have = apps
                .iter()
                .filter(|a| a["appType"] == "UPDATE" && a["owned"] == true)
                .filter_map(|a| a["appVersion"].as_u64())
                .max();
            let complete =
                apps.iter().filter(|a| a["appType"] == "DLC").all(|a| a["owned"] == true);
            title["ownership"] = if owned {
                json!({"haveBase":base,"upToDate":have>=latest,"complete":complete})
            } else {
                Value::Null
            };
            for key in [
                "releaseDate",
                "isDemo",
                "nsuId",
                "numberOfPlayers",
                "parentId",
                "rank",
                "rating",
                "size",
                "version",
            ] {
                if !title[key].is_null() && !title[key].is_string() {
                    title[key] = title[key].to_string().into();
                }
            }
            for key in ["category", "ratingContent", "regions", "languages", "screenshots", "ids"] {
                if let Some(raw) = title[key].as_str() {
                    title[key] = serde_json::from_str::<Value>(raw)
                        .ok()
                        .filter(Value::is_array)
                        .unwrap_or(Value::Null);
                }
            }
        }
        Ok(data)
    }
    pub fn title(&self, id: &Value) -> Value {
        self.titles.iter().find(|t| t["titleId"] == *id).cloned().unwrap_or(Value::Null)
    }
    pub fn stats(&self) -> Value {
        let owned =
            self.titles.iter().filter(|t| t["ownership"]["haveBase"] == true).collect::<Vec<_>>();
        let mut extensions = BTreeMap::<String, (u64, u64)>::new();
        let mut libraries = BTreeMap::<String, (u64, u64)>::new();
        let mut verdicts = BTreeMap::<String, (u64, u64)>::new();
        for lib in &self.libraries {
            libraries.insert(lib["path"].as_str().unwrap_or_default().into(), (0, 0));
        }
        for file in &self.files {
            let size = file["size"].as_u64().unwrap_or(0);
            for (map, key) in [
                (&mut extensions, file["extension"].as_str().unwrap_or_default()),
                (&mut libraries, file["_root"].as_str().unwrap_or_default()),
                (&mut verdicts, file["verificationStatus"].as_str().unwrap_or("UNVERIFIED")),
            ] {
                let bucket = map.entry(key.into()).or_default();
                bucket.0 += 1;
                bucket.1 += size;
            }
        }
        let buckets = |map: BTreeMap<String, (u64, u64)>| {
            map.into_iter()
                .map(|(key, (count, size))| json!({"key":key,"count":count,"size":size}))
                .collect::<Vec<_>>()
        };
        let types=["BASE","UPDATE","DLC"].map(|key|json!({"key":key,"count":self.apps.iter().filter(|a|a["appType"]==key).count(),"owned":self.apps.iter().filter(|a|a["appType"]==key && a["owned"]==true).count()}));
        let verdicts = [
            "VALID",
            "REPACK",
            "MODIFIED",
            "CORRUPT",
            "SIGNATURE_OK",
            "SIGNATURE_FAILED",
            "UNVERIFIED",
        ]
        .map(|status| {
            let (count, size) = verdicts.get(status).copied().unwrap_or_default();
            json!({"status":status,"count":count,"size":size})
        });
        json!({"totalFiles":if self.can_admin {self.files.len()}else{0},"totalSize":if self.can_admin {self.files.iter().filter_map(|f|f["size"].as_u64()).sum::<u64>()}else{0},"identifiedFiles":if self.can_admin {self.files.iter().filter(|f|f["identified"]==true).count()}else{0},"unidentifiedFiles":if self.can_admin {self.files.iter().filter(|f|f["identified"]!=true).count()}else{0},"totalTitles":self.titles.len(),"ownedTitles":owned.len(),"completeTitles":owned.iter().filter(|t|t["ownership"]["complete"]==true).count(),"upToDateTitles":owned.iter().filter(|t|t["ownership"]["upToDate"]==true).count(),"totalApps":self.apps.len(),"ownedApps":self.apps.iter().filter(|a|a["owned"]==true).count(),"appsByType":types,"filesByExtension":self.can_admin.then(||buckets(extensions)),"filesByLibrary":self.can_admin.then(||buckets(libraries)),"filesByVerificationStatus":self.can_admin.then_some(verdicts)})
    }
}
fn add_known(apps: &mut Vec<Value>, title: &str, id: &str, version: u64, kind: &str) {
    if !apps.iter().any(|a| a["appId"] == id && a["appVersion"] == version) {
        apps.push(json!({"id":format!("{id}:{version}"),"titleId":title,"appId":id,"appVersion":version,"appType":kind,"owned":false}));
    }
}
fn file_value(file: &crate::catalog::ContentFile, libraries: &[Value]) -> Value {
    let library =
        libraries.iter().find(|l| l["path"] == file.library_root.to_string_lossy().as_ref());
    json!({"id":file.id.to_string(),"libraryId":library.and_then(|l|l["id"].as_str()?.parse::<i64>().ok()).unwrap_or(0),"filename":file.name,"folder":file.relative_path.parent().map(|p|p.to_string_lossy()),"extension":file.relative_path.extension().and_then(|s|s.to_str()).unwrap_or_default(),"size":file.size,"compressed":matches!(file.relative_path.extension().and_then(|s|s.to_str()),Some("nsz"|"xcz")),"multicontent":file.is_multicontent(),"nbContent":file.identified_contents.len().max(usize::from(file.title_id.is_some())),"identified":file.title_id.is_some(),"identificationAttempts":0,"organized":false,"downloadCount":0,"verificationStatus":"UNVERIFIED","filepath":file.library_root.join(&file.relative_path),"_root":file.library_root})
}

pub fn matches(value: &Value, filter: &Value) -> bool {
    let Some(fields) = filter.as_object() else { return true };
    fields.iter().all(|(key, wanted)| {
        if wanted.is_null() {
            return true;
        }
        let actual = if matches!(key.as_str(), "haveBase" | "upToDate" | "complete") {
            value["ownership"].get(key).cloned().unwrap_or(json!(false))
        } else {
            value[key].clone()
        };
        let Some(operators) = wanted.as_object() else { return actual == *wanted };
        operators.iter().all(|(op, wanted)| {
            if wanted.is_null() {
                return true;
            }
            match op.as_str() {
                "eq" => actual == *wanted,
                "contains" => actual
                    .as_str()
                    .zip(wanted.as_str())
                    .is_some_and(|(a, b)| a.to_lowercase().contains(&b.to_lowercase())),
                "in" => {
                    wanted.as_array().is_some_and(|list| list.is_empty() || list.contains(&actual))
                }
                "gte" => actual.as_f64().zip(wanted.as_f64()).is_some_and(|(a, b)| a >= b),
                "lte" => actual.as_f64().zip(wanted.as_f64()).is_some_and(|(a, b)| a <= b),
                "has" => actual.as_array().is_some_and(|list| list.contains(wanted)),
                "hasAny" => wanted.as_array().is_some_and(|wanted| {
                    wanted.is_empty()
                        || actual
                            .as_array()
                            .is_some_and(|list| wanted.iter().any(|v| list.contains(v)))
                }),
                "hasAll" => wanted.as_array().is_some_and(|wanted| {
                    wanted.iter().all(|v| actual.as_array().is_some_and(|list| list.contains(v)))
                }),
                _ => false,
            }
        })
    })
}

#[allow(clippy::too_many_lines)] // Shared filtering and ordering for the public query contract.
pub fn select(
    mut rows: Vec<Value>,
    args: &Value,
    kind: &str,
    data: &GraphData,
    paginate: bool,
) -> Value {
    if kind == "App" && args["groupByAppId"] == true {
        let mut groups = BTreeMap::<String, Value>::new();
        for row in rows {
            let id = row["appId"].as_str().unwrap_or_default().to_string();
            if let Some(current) = groups.get_mut(&id) {
                let owned = current["owned"] == true || row["owned"] == true;
                if row["appVersion"].as_u64() > current["appVersion"].as_u64() {
                    *current = row;
                }
                current["owned"] = json!(owned);
            } else {
                groups.insert(id, row);
            }
        }
        rows = groups.into_values().collect();
    }
    rows.retain(|row| {
        let owned = if kind == "Title" {
            row["ownership"]["haveBase"] == true
        } else {
            row["owned"] == true
        };
        if args["owned"].as_bool().is_some_and(|wanted| wanted != owned) {
            return false;
        }
        if !matches(row, &args["filter"]) {
            return false;
        }
        let title = if kind == "App" { data.title(&row["titleId"]) } else { row.clone() };
        if let Some(types) = args["appType"].as_array() {
            if !types.is_empty() && !types.contains(&row["appType"]) {
                return false;
            }
        }
        if let Some(wanted) = args["complete"].as_bool() {
            if row["appType"] != "BASE" || (title["ownership"]["complete"] == true) != wanted {
                return false;
            }
        }
        if let Some(wanted) = args["upToDate"].as_bool() {
            let have = if row["appType"] == "BASE" {
                title["ownership"]["upToDate"] == true
            } else {
                let latest = data
                    .apps
                    .iter()
                    .filter(|a| a["appId"] == row["appId"])
                    .filter_map(|a| a["appVersion"].as_u64())
                    .max();
                data.apps.iter().any(|a| {
                    a["appId"] == row["appId"]
                        && a["owned"] == true
                        && a["appVersion"].as_u64() == latest
                })
            };
            if have != wanted {
                return false;
            }
        }
        if let Some(search) = args["search"].as_str() {
            let text = format!(
                "{} {} {} {}",
                title["name"].as_str().unwrap_or_default(),
                row["titleId"].as_str().unwrap_or_default(),
                row["appId"].as_str().unwrap_or_default(),
                row["filename"].as_str().unwrap_or_default()
            )
            .to_lowercase();
            if !text.contains(&search.to_lowercase()) {
                return false;
            }
        }
        true
    });
    let key = match args["orderBy"]["field"].as_str().unwrap_or("ID") {
        "NAME" => {
            if kind == "File" {
                "filename"
            } else {
                "name"
            }
        }
        "SIZE" => "size",
        "RELEASE_DATE" => {
            if kind == "File" {
                "mtime"
            } else {
                "releaseDate"
            }
        }
        "DOWNLOAD_COUNT" => "downloadCount",
        "ADDED_AT" => "addedAt",
        "VERSION" => "appVersion",
        _ => {
            if kind == "Title" {
                "titleId"
            } else {
                "id"
            }
        }
    };
    let sort_value = |row: &Value| {
        if kind == "App" && matches!(key, "name" | "releaseDate") {
            data.title(&row["titleId"])[key].clone()
        } else if key == "id" {
            row[key]
                .as_str()
                .and_then(|id| id.parse::<u64>().ok())
                .map_or_else(|| row[key].clone(), |id| json!(id))
        } else {
            row[key].clone()
        }
    };
    rows.sort_by(|a, b| {
        let av = sort_value(a);
        let bv = sort_value(b);
        let nulls = av.is_null().cmp(&bv.is_null());
        if nulls != std::cmp::Ordering::Equal {
            return nulls;
        }
        let mut order = if let (Some(a), Some(b)) = (av.as_u64(), bv.as_u64()) {
            a.cmp(&b)
        } else if av.is_number() && bv.is_number() {
            av.as_f64().partial_cmp(&bv.as_f64()).unwrap_or(std::cmp::Ordering::Equal)
        } else {
            av.as_str()
                .unwrap_or_default()
                .to_lowercase()
                .cmp(&bv.as_str().unwrap_or_default().to_lowercase())
        };
        if args["orderBy"]["direction"] == "DESC" {
            order = order.reverse();
        }
        order.then_with(|| {
            match (
                a["id"].as_str().and_then(|id| id.parse::<u64>().ok()),
                b["id"].as_str().and_then(|id| id.parse::<u64>().ok()),
            ) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => a["id"].to_string().cmp(&b["id"].to_string()),
            }
        })
    });
    if !paginate {
        return json!(rows);
    }
    let total = rows.len();
    let cap = if kind == "Title" { 500 } else { 1000 };
    let size = usize::try_from(
        args["pageSize"].as_i64().unwrap_or(if kind == "Title" { 50 } else { 100 }).clamp(1, cap),
    )
    .unwrap_or(100);
    let page = usize::try_from(args["page"].as_i64().unwrap_or(1).max(1)).unwrap_or(usize::MAX);
    json!({"total":total,"items":rows.into_iter().skip(page.saturating_sub(1).saturating_mul(size)).take(size).collect::<Vec<_>>()})
}
