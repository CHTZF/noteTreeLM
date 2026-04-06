use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::app_state::ApiState;

pub async fn list(
    State(state): State<ApiState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state.db
        .query("SELECT *, record::id(id) AS id FROM agent_tools ORDER BY is_builtin DESC, created_at DESC")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Value> = resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!(rows)))
}

pub async fn create(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let tool_id     = Uuid::new_v4().to_string();
    let name        = body["name"].as_str().unwrap_or("").to_string();
    let description = body["description"].as_str().unwrap_or("").to_string();
    let schema_json = body["schema_json"].as_str().map(|s| s.to_string());
    let is_builtin  = body["is_builtin"].as_bool().unwrap_or(false);
    let now = Utc::now().timestamp();

    state.db
        .query("INSERT INTO agent_tools (tool_id, name, description, schema_json, is_active, is_builtin, created_at) VALUES ($tid, $name, $desc, $schema, true, $builtin, $now)")
        .bind(("tid", tool_id.clone())).bind(("name", name)).bind(("desc", description))
        .bind(("schema", schema_json)).bind(("builtin", is_builtin)).bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "tool_id": tool_id })))
}

pub async fn update(
    State(state): State<ApiState>,
    Path(tool_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let name        = body["name"].as_str().unwrap_or("").to_string();
    let description = body["description"].as_str().unwrap_or("").to_string();
    let is_active   = body["is_active"].as_bool().unwrap_or(true);

    state.db
        .query("UPDATE agent_tools SET name = $name, description = $desc, is_active = $active WHERE tool_id = $tid")
        .bind(("name", name)).bind(("desc", description))
        .bind(("active", is_active)).bind(("tid", tool_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

pub async fn delete(
    State(state): State<ApiState>,
    Path(tool_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state.db
        .query("DELETE FROM agent_tools WHERE tool_id = $tid")
        .bind(("tid", tool_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}
