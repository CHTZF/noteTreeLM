use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get},
    Json, Router,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/scheduled-tasks",
            get(list_tasks).post(create_task),
        )
        .route("/scheduled-tasks/:task_id", delete(delete_task))
}

#[derive(Deserialize)]
struct ListQuery {
    vault_id: Option<String>,
}

async fn list_tasks(
    State(state): State<ApiState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let (query_str, maybe_vid) = if let Some(vid) = q.vault_id {
        (
            "SELECT *, record::id(id) AS id FROM scheduled_tasks WHERE vault_id = $vid ORDER BY run_at_ts ASC".to_string(),
            Some(vid),
        )
    } else {
        (
            "SELECT *, record::id(id) AS id FROM scheduled_tasks ORDER BY run_at_ts ASC".to_string(),
            None,
        )
    };

    let mut qb = state.db.query(&query_str);
    if let Some(vid) = maybe_vid {
        qb = qb.bind(("vid", vid));
    }

    let mut resp = qb
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn create_task(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let task_id = Uuid::new_v4().to_string();
    let vault_id = body
        .get("vault_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = body
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let agent_type = body
        .get("agent_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let agent_prompt = body
        .get("agent_prompt")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let run_at_ts = body
        .get("run_at_ts")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| Utc::now().timestamp());
    let repeat_interval_secs = body
        .get("repeat_interval_secs")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO scheduled_tasks (task_id, vault_id, description, agent_type, agent_prompt, run_at_ts, repeat_interval_secs, status, created_at) VALUES ($tid, $vid, $desc, $atype, $aprompt, $run_at, $interval, 'pending', $now)")
        .bind(("tid", task_id.clone()))
        .bind(("vid", vault_id))
        .bind(("desc", description))
        .bind(("atype", agent_type))
        .bind(("aprompt", agent_prompt))
        .bind(("run_at", run_at_ts))
        .bind(("interval", repeat_interval_secs))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "task_id": task_id })))
}

async fn delete_task(
    State(state): State<ApiState>,
    Path(task_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state
        .db
        .query("DELETE FROM scheduled_tasks WHERE task_id = $tid")
        .bind(("tid", task_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}
