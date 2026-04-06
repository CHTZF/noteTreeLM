use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::state::ApiState;
use super::account_id_from_headers;

pub async fn list(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let mut resp = state
        .db
        .query("SELECT *, record::id(id) AS id FROM agent_definitions WHERE account_id = $aid ORDER BY created_at DESC")
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Value> = resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!(rows)))
}

pub async fn create(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let def_id = Uuid::new_v4().to_string();
    let name        = body["name"].as_str().unwrap_or("").to_string();
    let description = body["description"].as_str().unwrap_or("").to_string();
    let kind        = body["kind"].as_str().unwrap_or("chat").to_string();
    let system_prompt = body["system_prompt"].as_str().map(|s| s.to_string());
    let max_rounds  = body["max_rounds"].as_i64().unwrap_or(10);
    let skill_ids: Option<Value> = body.get("skill_ids").cloned();
    let tool_names: Option<Value> = body.get("tool_names").cloned();
    let trigger     = body["trigger"].as_str().map(|s| s.to_string());
    let now = Utc::now().timestamp();

    state.db
        .query("INSERT INTO agent_definitions (def_id, account_id, name, description, kind, skill_ids, tool_names, system_prompt, max_rounds, is_active, is_builtin, trigger, created_at) VALUES ($did, $aid, $name, $desc, $kind, $skills, $tools, $prompt, $rounds, true, false, $trigger, $now)")
        .bind(("did", def_id.clone())).bind(("aid", account_id))
        .bind(("name", name)).bind(("desc", description)).bind(("kind", kind))
        .bind(("skills", skill_ids)).bind(("tools", tool_names))
        .bind(("prompt", system_prompt)).bind(("rounds", max_rounds))
        .bind(("trigger", trigger)).bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "def_id": def_id })))
}

pub async fn get_one(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let mut resp = state.db
        .query("SELECT *, record::id(id) AS id FROM agent_definitions WHERE def_id = $did AND account_id = $aid LIMIT 1")
        .bind(("did", def_id)).bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Value> = resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    rows.into_iter().next()
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "Agent definition not found".to_string()))
}

pub async fn update(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let mut set_parts = vec!["created_at = created_at".to_string()];

    macro_rules! maybe_set {
        ($field:literal, $key:literal) => {
            if body.get($key).is_some() { set_parts.push(format!("{} = ${}", $field, $key)); }
        };
    }
    maybe_set!("name", "name");
    maybe_set!("description", "description");
    maybe_set!("kind", "kind");
    maybe_set!("system_prompt", "system_prompt");
    maybe_set!("trigger", "trigger");
    set_parts.push("max_rounds = $max_rounds".to_string());

    let query = format!(
        "UPDATE agent_definitions SET {} WHERE def_id = $did AND account_id = $aid AND is_builtin = false",
        set_parts.join(", ")
    );
    let mut qb = state.db.query(&query)
        .bind(("aid", account_id)).bind(("did", def_id));
    if let Some(v) = body["name"].as_str()          { qb = qb.bind(("name", v.to_string())); }
    if let Some(v) = body["description"].as_str()   { qb = qb.bind(("description", v.to_string())); }
    if let Some(v) = body["kind"].as_str()           { qb = qb.bind(("kind", v.to_string())); }
    if let Some(v) = body["system_prompt"].as_str()  { qb = qb.bind(("system_prompt", v.to_string())); }
    if let Some(v) = body["trigger"].as_str()        { qb = qb.bind(("trigger", v.to_string())); }
    qb = qb.bind(("max_rounds", body["max_rounds"].as_i64().unwrap_or(10)));
    qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    state.db
        .query("DELETE FROM agent_definitions WHERE def_id = $did AND account_id = $aid AND is_builtin = false")
        .bind(("did", def_id)).bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn record_usage(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let now = Utc::now().timestamp();
    state.db
        .query("UPDATE agent_definitions SET use_count = (use_count OR 0) + 1, last_used_at = $now, status = 'active', slept_at = NONE WHERE def_id = $did AND account_id = $aid")
        .bind(("now", now)).bind(("did", def_id)).bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn wake(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    state.db
        .query("UPDATE agent_definitions SET status = 'active', slept_at = NONE WHERE def_id = $did AND account_id = $aid AND is_builtin = false")
        .bind(("did", def_id)).bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn lifecycle(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let now = Utc::now().timestamp();
    let sleep_cutoff  = now - 7  * 86400;
    let delete_cutoff = now - 23 * 86400;

    state.db
        .query("DELETE agent_definitions WHERE account_id = $aid AND is_builtin = false AND status = 'sleep' AND slept_at != NONE AND slept_at < $dcutoff")
        .bind(("aid", account_id.clone())).bind(("dcutoff", delete_cutoff))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.db
        .query("UPDATE agent_definitions SET status = 'sleep', slept_at = $now WHERE account_id = $aid AND is_builtin = false AND status != 'sleep' AND ((last_used_at != NONE AND last_used_at < $scutoff) OR (last_used_at = NONE AND created_at < $scutoff))")
        .bind(("aid", account_id)).bind(("now", now)).bind(("scutoff", sleep_cutoff))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

pub async fn toggle(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let is_active = body["is_active"].as_bool().unwrap_or(true);
    state.db
        .query("UPDATE agent_definitions SET is_active = $active WHERE def_id = $did AND account_id = $aid")
        .bind(("active", is_active)).bind(("did", def_id)).bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}
