use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/conversations", get(list_conversations).post(create_conversation))
        .route(
            "/conversations/:id",
            get(get_conversation).delete(delete_conversation),
        )
        .route("/conversations/:id/title", patch(update_title))
        .route("/conversations/:id/messages", put(save_messages))
}

#[derive(Deserialize)]
struct ListQuery {
    vault_id: Option<String>,
    mode: Option<String>,
}

async fn list_conversations(
    State(state): State<ApiState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut query_str = "SELECT * FROM conversations".to_string();
    let mut conditions = Vec::new();
    if q.vault_id.is_some() {
        conditions.push("vault_id = $vault_id");
    }
    if q.mode.is_some() {
        conditions.push("mode = $mode");
    }
    if !conditions.is_empty() {
        query_str.push_str(" WHERE ");
        query_str.push_str(&conditions.join(" AND "));
    }
    query_str.push_str(" ORDER BY updated_at DESC");

    let mut qb = state.db.query(&query_str);
    if let Some(vid) = q.vault_id {
        qb = qb.bind(("vault_id", vid));
    }
    if let Some(m) = q.mode {
        qb = qb.bind(("mode", m));
    }

    let mut resp = qb
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn create_conversation(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let account_id = body
        .get("account_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let vault_id = body
        .get("vault_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mode = body
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("chat")
        .to_string();
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("New Conversation")
        .to_string();
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO conversations (id, account_id, vault_id, mode, title, messages_json, memory_processed, created_at, updated_at) VALUES ($id, $account_id, $vault_id, $mode, $title, '[]', false, $now, $now)")
        .bind(("id", id.clone()))
        .bind(("account_id", account_id))
        .bind(("vault_id", vault_id))
        .bind(("mode", mode))
        .bind(("title", title))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "id": id })))
}

async fn get_conversation(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM conversations WHERE id = $id LIMIT 1")
        .bind(("id", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match rows.into_iter().next() {
        Some(row) => Ok(Json(row)),
        None => Err((StatusCode::NOT_FOUND, "Conversation not found".to_string())),
    }
}

async fn delete_conversation(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state
        .db
        .query("DELETE FROM conversations WHERE id = $id")
        .bind(("id", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn update_title(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing title".to_string()))?
        .to_string();
    let now = Utc::now().timestamp();

    state
        .db
        .query("UPDATE conversations SET title = $title, updated_at = $now WHERE id = $id")
        .bind(("title", title))
        .bind(("now", now))
        .bind(("id", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn save_messages(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let messages = body
        .get("messages")
        .ok_or((StatusCode::BAD_REQUEST, "Missing messages".to_string()))?;
    let messages_json = serde_json::to_string(messages)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let now = Utc::now().timestamp();

    state
        .db
        .query("UPDATE conversations SET messages_json = $mj, updated_at = $now WHERE id = $id")
        .bind(("mj", messages_json))
        .bind(("now", now))
        .bind(("id", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}
