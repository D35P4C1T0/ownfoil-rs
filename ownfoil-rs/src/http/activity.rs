//! Admin activity pages and multiplexed realtime snapshots.
use super::{
    AppState,
    auth::{Access, ensure_access},
    error::ApiError,
};
use axum::{
    extract::{
        Query, State,
        ws::{Message, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use axum_extra::extract::CookieJar;
use serde_json::{Value, json};

async fn access(state: &AppState, headers: &HeaderMap, jar: &CookieJar) -> Result<(), ApiError> {
    ensure_access(state, headers, super::handlers::session_token(jar), Access::Admin).await
}
pub async fn tasks_page(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    access(&state, &headers, &jar).await?;
    Ok(super::handlers::html_page(include_str!("tasks.html")))
}
pub async fn stats_page(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    access(&state, &headers, &jar).await?;
    Ok(super::handlers::html_page(include_str!("stats.html")))
}
pub async fn websocket(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    access(&state, &headers, &jar).await?;
    super::handlers::ensure_same_origin(&headers)?;
    let topics = params
        .get("topics")
        .map_or("tasks,workers", String::as_str)
        .split(',')
        .filter(|name| matches!(*name, "tasks" | "workers"))
        .map(str::to_string)
        .collect::<Vec<_>>();
    Ok(upgrade.on_upgrade(move |mut socket|async move {
        let mut previous=std::collections::HashMap::<String,Vec<Value>>::new();
        let mut ticker=tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            tokio::select! {
                message=socket.recv()=>{if !matches!(message,Some(Ok(Message::Ping(_)|Message::Pong(_)|Message::Text(_)|Message::Binary(_)))) {break;}},
                _=ticker.tick()=>{
                    // Revoked sessions stop receiving privileged filesystem/task data.
                    if access(&state,&headers,&jar).await.is_err() {break;}
                    for topic in &topics {
                        let rows=if topic=="workers" {crate::tasks::workers(&state).await} else if let Some(storage)=&state.storage {crate::tasks::list(storage).await} else {Ok(Vec::new())};
                        let Ok(mut rows)=rows else {break;};
                        if topic == "tasks" { rows.reverse(); }
                        rows.truncate(200);
                        for row in &mut rows { normalize_row(topic, row); }
                        let events = topic_events(topic, previous.get(topic).map(Vec::as_slice), &rows);
                        previous.insert(topic.clone(), rows);
                        for event in events {
                            let sent = tokio::time::timeout(std::time::Duration::from_secs(10), socket.send(Message::Text(event.to_string().into()))).await;
                            if !matches!(sent, Ok(Ok(()))) { return; }
                        }
                    }
                }
            }
        }
    }).into_response())
}

fn numeric_id(value: &Value) -> Value {
    value.as_i64().or_else(|| value.as_str()?.parse().ok()).map_or(Value::Null, Value::from)
}

fn normalize_row(topic: &str, row: &mut Value) {
    if topic == "tasks" {
        row["id"] = numeric_id(&row["id"]);
        row["parentId"] = numeric_id(&row["parentId"]);
        row["status"] = row["status"].as_str().unwrap_or_default().to_ascii_lowercase().into();
    } else {
        row["taskId"] = numeric_id(&row["currentTask"]["id"]);
        if let Some(object) = row.as_object_mut() {
            object.remove("currentTask");
        }
    }
}

fn topic_events(topic: &str, previous: Option<&[Value]>, current: &[Value]) -> Vec<Value> {
    let Some(previous) = previous else {
        return vec![json!({"topic":topic,"type":"snapshot","data":current})];
    };
    let mut events = Vec::new();
    for row in current {
        let old = previous.iter().find(|old| old["id"] == row["id"]);
        let kind = match old {
            None => "add",
            Some(old) if old != row => "update",
            Some(_) => continue,
        };
        events.push(json!({"topic":topic,"type":kind,"data":row}));
    }
    for row in previous {
        if !current.iter().any(|new| new["id"] == row["id"]) {
            events.push(json!({"topic":topic,"type":"remove","data":row}));
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realtime_contract_uses_numeric_ids_and_row_deltas() {
        let mut task = json!({"id":"12","parentId":"3","status":"RUNNING"});
        normalize_row("tasks", &mut task);
        assert_eq!(task, json!({"id":12,"parentId":3,"status":"running"}));
        let mut worker = json!({"id":1,"currentTask":{"id":"12"}});
        normalize_row("workers", &mut worker);
        assert_eq!(worker, json!({"id":1,"taskId":12}));
        let old = vec![json!({"id":1,"status":"pending"}), json!({"id":2})];
        let new = vec![json!({"id":1,"status":"running"}), json!({"id":3})];
        assert_eq!(topic_events("tasks", None, &old)[0]["type"], "snapshot");
        let events = topic_events("tasks", Some(&old), &new);
        assert_eq!(
            events.iter().map(|e| e["type"].as_str().unwrap_or_default()).collect::<Vec<_>>(),
            ["update", "add", "remove"]
        );
        assert_eq!(events[2]["data"], old[1]);
        assert!(topic_events("tasks", Some(&new), &new).is_empty());
    }
}
