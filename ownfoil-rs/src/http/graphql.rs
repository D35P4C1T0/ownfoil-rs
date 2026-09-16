//! Upstream GraphQL contract executed by async-graphql, including validation and introspection.
use super::{
    AppState,
    auth::extract_basic_auth,
    error::ApiError,
    graph_data::{GraphData, select},
};
use async_graphql::{
    Name, Request, Value as GqlValue,
    dynamic::{
        Enum, EnumItem, Field, FieldFuture, FieldValue, InputObject, InputValue, Object, Scalar,
        Schema, TypeRef,
    },
};
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use axum_extra::extract::CookieJar;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

#[derive(Clone)]
struct Context {
    state: AppState,
    data: GraphData,
}
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    build_schema().unwrap_or_else(|error| panic!("Invalid GraphQL contract: {error}"))
});

#[allow(clippy::option_if_let_else)] // Recursive syntax parsing reads directly as grammar cases.
fn type_ref(raw: &str) -> TypeRef {
    if let Some(raw) = raw.strip_suffix('!') {
        TypeRef::NonNull(Box::new(type_ref(raw)))
    } else if let Some(raw) = raw.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        TypeRef::List(Box::new(type_ref(raw)))
    } else {
        TypeRef::named(raw)
    }
}
fn input_value(field: &Value) -> InputValue {
    let mut input = InputValue::new(
        field["name"].as_str().unwrap_or_default(),
        type_ref(field["type"].as_str().unwrap_or("String")),
    );
    if let Some(value) = field.get("default") {
        if !value.is_null() {
            if let Ok(mut value) = GqlValue::from_json(value.clone()) {
                if matches!(field["type"].as_str(), Some("OrderField!" | "OrderDirection!")) {
                    if let GqlValue::String(name) = value {
                        value = GqlValue::Enum(Name::new(name));
                    }
                }
                input = input.default_value(value);
            }
        }
    }
    input
}
fn build_schema() -> anyhow::Result<Schema> {
    let contract: Vec<Value> = serde_json::from_str(include_str!("graphql_contract.json"))?;
    let mut schema =
        Schema::build("Query", Some("Mutation"), None).limit_depth(15).limit_complexity(50_000);
    schema = schema.register(Scalar::new("BigInt").validator(
        |v| matches!(v,GqlValue::Number(n) if n.as_i64().is_some() || n.as_u64().is_some()),
    ));
    for item in &contract {
        let name = item["name"].as_str().unwrap_or_default();
        let fields = item["fields"].as_array().context("missing contract fields")?;
        match item["kind"].as_str() {
            Some("enum") => {
                let mut enumeration = Enum::new(name);
                for field in fields {
                    enumeration =
                        enumeration.item(EnumItem::new(field["name"].as_str().unwrap_or_default()));
                }
                schema = schema.register(enumeration);
            }
            Some("input") => {
                let mut input = InputObject::new(name);
                for field in fields {
                    input = input.field(input_value(field));
                }
                schema = schema.register(input);
            }
            _ => {
                let mut object =
                    Object::new(name).description(item["description"].as_str().unwrap_or_default());
                for definition in fields {
                    let field_name = definition["name"].as_str().unwrap_or_default().to_string();
                    let owner = name.to_string();
                    let property = field_name.clone();
                    let result_type = definition["type"].as_str().unwrap_or("String").to_string();
                    let field_type = result_type.clone();
                    let mut field = Field::new(field_name, type_ref(&result_type), move |ctx| {
                        let owner = owner.clone();
                        let property = property.clone();
                        let field_type = field_type.clone();
                        FieldFuture::new(async move {
                            let context = ctx.data::<Context>()?;
                            let args = Value::Object(
                                ctx.args
                                    .iter()
                                    .map(|(key, value)| {
                                        Ok((key.to_string(), value.deserialize::<Value>()?))
                                    })
                                    .collect::<async_graphql::Result<_>>()?,
                            );
                            let parent = ctx
                                .parent_value
                                .try_downcast_ref::<Value>()
                                .cloned()
                                .unwrap_or(Value::Null);
                            let value = resolve(context, &owner, &property, &parent, &args)
                                .await
                                .map_err(|e| async_graphql::Error::new(e.to_string()))?;
                            Ok(Some(convert(value, &field_type)))
                        })
                    });
                    if let Some(args) = definition["args"].as_array() {
                        for arg in args {
                            field = field.argument(input_value(arg));
                        }
                    }
                    if let Some(description) = definition["description"].as_str() {
                        field = field.description(description);
                    }
                    object = object.field(field);
                }
                schema = schema.register(object);
            }
        }
    }
    Ok(schema.finish()?)
}
use anyhow::{Context as _, bail, ensure};
fn convert(value: Value, raw: &str) -> FieldValue<'static> {
    if value.is_null() {
        return FieldValue::NULL;
    }
    let raw = raw.trim_end_matches('!');
    if let Some(inner) = raw.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return FieldValue::list(
            value.as_array().into_iter().flatten().cloned().map(|value| convert(value, inner)),
        );
    }
    if matches!(raw, "String" | "ID" | "Boolean" | "Int" | "BigInt" | "Float") {
        return FieldValue::value(GqlValue::from_json(value).unwrap_or(GqlValue::Null));
    }
    if value.is_string() {
        return FieldValue::value(GqlValue::Enum(Name::new(value.as_str().unwrap_or_default())));
    }
    FieldValue::owned_any(value)
}
async fn resolve(
    ctx: &Context,
    owner: &str,
    field: &str,
    parent: &Value,
    args: &Value,
) -> anyhow::Result<Value> {
    let data = &ctx.data;
    if owner == "Mutation" {
        return mutate(ctx, field, args).await;
    }
    if owner == "Query" {
        return Ok(match field {
            "titles" => select(data.titles.clone(), args, "Title", data, true),
            "apps" => select(data.apps.clone(), args, "App", data, true),
            "files" => select(
                if data.can_admin { data.files.clone() } else { Vec::new() },
                args,
                "File",
                data,
                true,
            ),
            "title" => data.title(&args["titleId"]),
            "app" => {
                data.apps.iter().find(|v| v["id"] == args["id"]).cloned().unwrap_or(Value::Null)
            }
            "file" => {
                if data.can_admin {
                    data.files
                        .iter()
                        .find(|v| v["id"] == args["id"])
                        .cloned()
                        .unwrap_or(Value::Null)
                } else {
                    Value::Null
                }
            }
            "libraries" => json!(if data.can_admin { data.libraries.clone() } else { Vec::new() }),
            "workers" => json!(if data.can_admin { data.workers.clone() } else { Vec::new() }),
            "tasks" => {
                if let Some(name) = args["taskName"].as_str() {
                    ensure!(crate::tasks::NAMES.contains(&name), "Unknown task: {name}");
                }
                json!(
                    data.tasks
                        .iter()
                        .filter(|t| data.can_admin
                            && (args["status"].is_null() || t["status"] == args["status"])
                            && (args["taskName"].is_null() || t["taskName"] == args["taskName"])
                            && (args["includeChildren"] == true || t["parentId"].is_null()))
                        .take(
                            usize::try_from(args["limit"].as_i64().unwrap_or(50).clamp(1, 500))
                                .unwrap_or(50)
                        )
                        .collect::<Vec<_>>()
                )
            }
            "task" => {
                if data.can_admin {
                    data.tasks
                        .iter()
                        .find(|v| v["id"] == args["id"])
                        .cloned()
                        .unwrap_or(Value::Null)
                } else {
                    Value::Null
                }
            }
            "stats" => data.stats(),
            _ => Value::Null,
        });
    }
    match (owner,field) {
        ("Title","apps")=>Ok(select(data.apps.iter().filter(|a|a["titleId"]==parent["titleId"]).cloned().collect(),args,"App",data,false)),
        ("Title","availableVersions")=>Ok(json!(ctx.state.titledb.versions(parent["titleId"].as_str().unwrap_or_default()).await.map(|v|v.versions.into_iter().map(|version|json!({"version":version,"releaseDate":v.release_dates.get(&version)})).collect::<Vec<_>>()).unwrap_or_default())),
        ("Title","availableDlc")=>Ok(json!(ctx.state.titledb.dlc_for_title(parent["titleId"].as_str().unwrap_or_default()).await.into_iter().map(|v|json!({"appId":v.title_id,"version":v.version})).collect::<Vec<_>>())),
        ("Title","ncaKey")=>Ok(parent["key"].clone()),
        ("TitledbDlc" | "App", "titledb")=>Ok(data.title(&parent["appId"])),
        ("App","title")=>Ok(data.title(&parent["titleId"])),
        ("App","versions")=>{
            let id=if parent["appType"]=="BASE" {format!("{}800",parent["titleId"].as_str().unwrap_or_default().get(..13).unwrap_or_default())}else{parent["appId"].as_str().unwrap_or_default().into()};
            let mut rows=data.apps.iter().filter(|a|a["appId"]==id).map(|a|json!({"version":a["appVersion"],"owned":a["owned"],"releaseDate":a["releaseDate"]})).collect::<Vec<_>>();rows.sort_by_key(|v|v["version"].as_u64());Ok(json!(rows))
        }
        ("App","files")=>if data.can_admin {Ok(select(data.files.iter().filter(|f|parent["_fileIds"].as_array().is_some_and(|ids|ids.contains(&f["id"]))).cloned().collect(),args,"File",data,false))}else{Ok(Value::Null)},
        ("File","apps")=>Ok(select(data.apps.iter().filter(|a|a["_fileIds"].as_array().is_some_and(|ids|ids.contains(&parent["id"]))).cloned().collect(),args,"App",data,false)),
        ("File","library")=>Ok(data.libraries.iter().find(|l|l["id"].as_str().and_then(|s|s.parse::<i64>().ok())==parent["libraryId"].as_i64()).cloned().unwrap_or(Value::Null)),
        ("Task","children")=>Ok(json!(data.tasks.iter().filter(|t|t["parentId"]==parent["id"]).collect::<Vec<_>>())),
        _=>Ok(parent[field].clone()),
    }
}
#[allow(clippy::too_many_lines)] // Dispatch the public mutation contract with shared authorization.
async fn mutate(ctx: &Context, field: &str, args: &Value) -> anyhow::Result<Value> {
    ensure!(ctx.data.can_admin, "Admin access is required for this operation.");
    let storage = ctx.state.storage.as_ref().context("Storage unavailable")?;
    let id = || {
        args["id"].as_str().context("id is required")?.parse::<i64>().map_err(anyhow::Error::from)
    };
    match field {
        "enqueueTask" => {
            crate::tasks::enqueue(
                storage,
                args["name"].as_str().unwrap_or_default(),
                serde_json::from_str(args["input"].as_str().unwrap_or("{}"))?,
            )
            .await
        }
        "cancelTask" => Ok(json!(crate::tasks::cancel(storage, id()?).await?)),
        "dismissTask" => Ok(json!(crate::tasks::dismiss(storage, Some(id()?)).await? > 0)),
        "purgeFailedTasks" => Ok(json!(crate::tasks::dismiss(storage, None).await?)),
        "scanLibrary" => {
            crate::tasks::enqueue(
                storage,
                "scan_library",
                if let Some(path) = args["path"].as_str() {
                    ensure!(
                        ctx.state
                            .settings
                            .read()
                            .await
                            .library
                            .paths
                            .contains(&std::path::PathBuf::from(path)),
                        "Unknown library path"
                    );
                    json!({"library_path":path})
                } else {
                    json!({})
                },
            )
            .await
        }
        "compressFile" | "decompressFile" | "verifyFile" => {
            let file_id = args["fileId"].as_str().context("fileId is required")?.parse::<i64>()?;
            let file_id_text = file_id.to_string();
            let file = ctx
                .data
                .files
                .iter()
                .find(|f| f["id"] == file_id_text)
                .context("File not found")?;
            if field == "compressFile" {
                ensure!(file["compressed"] != true, "File is already compressed");
                ensure!(
                    matches!(file["extension"].as_str(), Some("nsp" | "xci")),
                    "File type cannot be compressed"
                );
            }
            if field == "decompressFile" {
                ensure!(file["compressed"] == true, "File is not compressed");
            }
            let name = match field {
                "compressFile" => "compress_file",
                "decompressFile" => "decompress_file",
                _ => "verify_file",
            };
            crate::tasks::enqueue(storage, name, json!({"file_id":file_id})).await
        }
        "setTitleOverride" | "deleteTitleOverride" => {
            let title_id =
                args["titleId"].as_str().context("titleId is required")?.to_ascii_uppercase();
            ensure!(
                title_id.len() == 16 && title_id.chars().all(|c| c.is_ascii_hexdigit()),
                "Invalid title id"
            );
            if field == "deleteTitleOverride" {
                let delete_id = title_id.clone();
                let removed = storage
                    .with_connection(move |conn| {
                        Ok(conn.execute(
                            "DELETE FROM title_overrides WHERE title_id=?1",
                            [delete_id],
                        )? > 0)
                    })
                    .await?;
                let records = storage.title_override_records().await?;
                let record =
                    records.iter().find(|(id, _)| id == &title_id).map(|(_, record)| record);
                ctx.state.titledb.set_override(&title_id, record).await?;
                return Ok(json!(removed));
            }
            let record: Value =
                serde_json::from_str(args["record"].as_str().context("record is required")?)?;
            ensure!(record.is_object(), "record must be a JSON object");
            let save_id = title_id.clone();
            let save_record = record.to_string();
            storage.with_connection(move |conn| {conn.execute("INSERT INTO title_overrides(title_id,record) VALUES(?1,?2) ON CONFLICT(title_id) DO UPDATE SET record=excluded.record",rusqlite::params![save_id,save_record])?;Ok(())}).await?;
            let records = storage.title_override_records().await?;
            let record = records.iter().find(|(id, _)| id == &title_id).map(|(_, record)| record);
            ctx.state.titledb.set_override(&title_id, record).await?;
            crate::tasks::enqueue(storage, "process_library", json!({})).await?;
            let refreshed = GraphData::load(&ctx.state, true).await?;
            Ok(refreshed.title(&json!(title_id)))
        }
        _ => bail!("Unknown mutation"),
    }
}

pub async fn post(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(request): Json<Request>,
) -> Result<Response, ApiError> {
    dispatch(state, jar, headers, request, false).await
}
pub async fn get(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, ApiError> {
    graph_access(&state, &headers, &jar)?;
    let Some(query) = params.get("query") else {
        return Ok(axum::response::Html(
            async_graphql::http::GraphiQLSource::build().endpoint("/api/graphql").finish(),
        )
        .into_response());
    };
    let mut request = Request::new(query);
    if let Some(name) = params.get("operationName") {
        request = request.operation_name(name);
    }
    if let Some(raw) = params.get("variables") {
        request = request.variables(async_graphql::Variables::from_json(
            serde_json::from_str(raw).map_err(|_| ApiError::InvalidPath)?,
        ));
    }
    dispatch(state, jar, headers, request, true).await
}
async fn dispatch(
    state: AppState,
    jar: CookieJar,
    headers: HeaderMap,
    mut request: Request,
    is_get: bool,
) -> Result<Response, ApiError> {
    let can_admin = graph_access(&state, &headers, &jar)?;
    let mutation = async_graphql_parser::parse_query(&request.query).ok().is_some_and(|doc| {
        doc.operations.iter().any(|(name, op)| {
            request
                .operation_name
                .as_deref()
                .is_none_or(|selected| name.is_some_and(|name| name.as_str() == selected))
                && op.node.ty == async_graphql_parser::types::OperationType::Mutation
        })
    });
    if mutation && is_get {
        return Ok(
            (StatusCode::METHOD_NOT_ALLOWED, "Mutations must be sent by POST").into_response()
        );
    }
    if mutation {
        super::handlers::ensure_same_origin(&headers)?;
    }
    let data = GraphData::load(&state, can_admin).await.map_err(|error| {
        tracing::error!(%error,"GraphQL snapshot failed");
        ApiError::Internal
    })?;
    request = request.data(Context { state, data });
    let result = SCHEMA.execute(request).await;
    let encoded = serde_json::to_vec(&result).map_err(|_| ApiError::Internal)?;
    let etag = format!("\"{}\"", hex::encode(Sha256::digest(&encoded)));
    let unchanged = !mutation
        && headers
            .get("if-none-match")
            .and_then(|s| s.to_str().ok())
            .is_some_and(|s| s.split(',').any(|v| v.trim() == etag));
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Json(result).into_response()
    };
    response.headers_mut().insert(
        "cache-control",
        if mutation { "no-store" } else { "private, must-revalidate" }
            .parse()
            .map_err(|_| ApiError::Internal)?,
    );
    response
        .headers_mut()
        .insert("vary", "Authorization, Cookie".parse().map_err(|_| ApiError::Internal)?);
    if !mutation {
        response.headers_mut().insert("etag", etag.parse().map_err(|_| ApiError::Internal)?);
    }
    Ok(response)
}

fn graph_access(state: &AppState, headers: &HeaderMap, jar: &CookieJar) -> Result<bool, ApiError> {
    if !state.auth.is_enabled() {
        return Ok(true);
    }
    let username = super::handlers::session_token(jar)
        .and_then(|token| state.sessions.get(token))
        .or_else(|| {
            extract_basic_auth(headers).and_then(|(name, password)| {
                state.auth.is_authorized(&name, &password).then_some(name)
            })
        })
        .ok_or(ApiError::Unauthorized)?;
    let roles = state.auth.roles(&username).ok_or(ApiError::Unauthorized)?;
    if !roles.admin_access && !roles.shop_access {
        return Err(ApiError::Forbidden);
    }
    Ok(roles.admin_access)
}
