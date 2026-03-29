use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, patch, post, put},
    Json, Router,
};
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api_state::ApiState;
use crate::routes::auth::extract_bearer;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/agents", get(list_agent_definitions).post(create_agent_definition))
        .route("/agents/:def_id", get(get_agent_definition).put(update_agent_definition).delete(delete_agent_definition))
        .route("/agents/:def_id/usage", post(record_agent_usage))
        .route("/agents/:def_id/wake", post(wake_agent))
        .route("/agents/:def_id/toggle", patch(toggle_agent))
        .route("/agents/lifecycle", post(run_agent_lifecycle))
        .route("/skills", get(list_agent_skills).post(create_agent_skill))
        .route("/skills/:skill_id", get(get_agent_skill).put(update_agent_skill).delete(delete_agent_skill))
        .route("/skills/:skill_id/toggle", patch(toggle_agent_skill))
        .route("/skills/:skill_id/trigger", post(bump_skill_trigger))
        .route("/skills/seed-builtins", post(seed_builtins))
        .route("/agent-tools", get(list_agent_tools).post(create_agent_tool))
        .route("/agent-tools/:tool_id", put(update_agent_tool).delete(delete_agent_tool))
        // ── Interactive agent run/cancel/confirm ─────────────────────────────
        .route("/vaults/:vid/agent/run", post(agent_run))
        .route("/vaults/:vid/agent/cancel", post(agent_cancel))
        .route("/vaults/:vid/agent/confirm", post(agent_confirm))
}

/// Resolve account_id from Bearer token (username from sessions table).
async fn account_id_from_headers(state: &ApiState, headers: &HeaderMap) -> Result<String, (StatusCode, String)> {
    let token = extract_bearer(headers)
        .ok_or((StatusCode::UNAUTHORIZED, "Missing token".to_string()))?;
    let now = Utc::now().timestamp();
    #[derive(serde::Deserialize)]
    struct Row { username: String }
    let mut r = state.db
        .query("SELECT username FROM sessions WHERE token = $t AND expires_at > $now LIMIT 1")
        .bind(("t", token))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    r.take::<Vec<Row>>(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter().next()
        .map(|r| r.username)
        .ok_or((StatusCode::UNAUTHORIZED, "Invalid or expired session".to_string()))
}

// ── Agent Definitions ────────────────────────────────────────────────────────

async fn list_agent_definitions(
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

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn create_agent_definition(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let def_id = Uuid::new_v4().to_string();
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let description = body.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let kind = body.get("kind").and_then(|v| v.as_str()).unwrap_or("chat").to_string();
    let system_prompt = body.get("system_prompt").and_then(|v| v.as_str()).map(|s| s.to_string());
    let max_rounds = body.get("max_rounds").and_then(|v| v.as_i64()).unwrap_or(10);
    let skill_ids: Option<Value> = body.get("skill_ids").cloned();
    let tool_names: Option<Value> = body.get("tool_names").cloned();
    let trigger = body.get("trigger").and_then(|v| v.as_str()).map(|s| s.to_string());
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO agent_definitions (def_id, account_id, name, description, kind, skill_ids, tool_names, system_prompt, max_rounds, is_active, is_builtin, trigger, created_at) VALUES ($did, $aid, $name, $desc, $kind, $skills, $tools, $prompt, $rounds, true, false, $trigger, $now)")
        .bind(("did", def_id.clone()))
        .bind(("aid", account_id))
        .bind(("name", name))
        .bind(("desc", description))
        .bind(("kind", kind))
        .bind(("skills", skill_ids))
        .bind(("tools", tool_names))
        .bind(("prompt", system_prompt))
        .bind(("rounds", max_rounds))
        .bind(("trigger", trigger))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "def_id": def_id })))
}

async fn get_agent_definition(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let mut resp = state
        .db
        .query("SELECT *, record::id(id) AS id FROM agent_definitions WHERE def_id = $did AND account_id = $aid LIMIT 1")
        .bind(("did", def_id))
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match rows.into_iter().next() {
        Some(row) => Ok(Json(row)),
        None => Err((StatusCode::NOT_FOUND, "Agent definition not found".to_string())),
    }
}

async fn update_agent_definition(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let mut set_parts = vec!["created_at = created_at".to_string()];

    macro_rules! maybe_set_str {
        ($field:literal, $key:literal) => {
            if body.get($key).is_some() {
                set_parts.push(format!("{} = ${}", $field, $key));
            }
        };
    }
    maybe_set_str!("name", "name");
    maybe_set_str!("description", "description");
    maybe_set_str!("kind", "kind");
    maybe_set_str!("system_prompt", "system_prompt");
    maybe_set_str!("trigger", "trigger");
    set_parts.push("max_rounds = $max_rounds".to_string());

    let set_clause = set_parts.join(", ");
    let query = format!("UPDATE agent_definitions SET {set_clause} WHERE def_id = $did AND account_id = $aid AND is_builtin = false");

    let mut qb = state.db.query(&query)
        .bind(("aid", account_id))
        .bind(("did", def_id));
    if let Some(v) = body.get("name").and_then(|v| v.as_str())          { qb = qb.bind(("name", v.to_string())); }
    if let Some(v) = body.get("description").and_then(|v| v.as_str())   { qb = qb.bind(("description", v.to_string())); }
    if let Some(v) = body.get("kind").and_then(|v| v.as_str())          { qb = qb.bind(("kind", v.to_string())); }
    if let Some(v) = body.get("system_prompt").and_then(|v| v.as_str()) { qb = qb.bind(("system_prompt", v.to_string())); }
    if let Some(v) = body.get("trigger").and_then(|v| v.as_str())       { qb = qb.bind(("trigger", v.to_string())); }
    let max_rounds = body.get("max_rounds").and_then(|v| v.as_i64()).unwrap_or(10);
    qb = qb.bind(("max_rounds", max_rounds));

    qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete_agent_definition(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    state
        .db
        .query("DELETE FROM agent_definitions WHERE def_id = $did AND account_id = $aid AND is_builtin = false")
        .bind(("did", def_id))
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn record_agent_usage(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let now = Utc::now().timestamp();
    state
        .db
        .query("UPDATE agent_definitions SET use_count = (use_count OR 0) + 1, last_used_at = $now, status = 'active', slept_at = NONE WHERE def_id = $did AND account_id = $aid")
        .bind(("now", now))
        .bind(("did", def_id))
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn wake_agent(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    state
        .db
        .query("UPDATE agent_definitions SET status = 'active', slept_at = NONE WHERE def_id = $did AND account_id = $aid AND is_builtin = false")
        .bind(("did", def_id))
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn run_agent_lifecycle(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let now = Utc::now().timestamp();
    let sleep_cutoff = now - 7 * 86400;
    let delete_cutoff = now - 23 * 86400;

    state
        .db
        .query("DELETE agent_definitions WHERE account_id = $aid AND is_builtin = false AND status = 'sleep' AND slept_at != NONE AND slept_at < $dcutoff")
        .bind(("aid", account_id.clone()))
        .bind(("dcutoff", delete_cutoff))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state
        .db
        .query("UPDATE agent_definitions SET status = 'sleep', slept_at = $now WHERE account_id = $aid AND is_builtin = false AND status != 'sleep' AND ((last_used_at != NONE AND last_used_at < $scutoff) OR (last_used_at = NONE AND created_at < $scutoff))")
        .bind(("aid", account_id))
        .bind(("now", now))
        .bind(("scutoff", sleep_cutoff))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn toggle_agent(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(def_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let is_active = body.get("is_active").and_then(|v| v.as_bool()).unwrap_or(true);
    state
        .db
        .query("UPDATE agent_definitions SET is_active = $active WHERE def_id = $did AND account_id = $aid")
        .bind(("active", is_active))
        .bind(("did", def_id))
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

// ── Agent Skills ─────────────────────────────────────────────────────────────

async fn list_agent_skills(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let mut resp = state
        .db
        .query("SELECT *, record::id(id) AS id FROM agent_skills WHERE account_id = $aid ORDER BY created_at DESC")
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn create_agent_skill(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let skill_id = Uuid::new_v4().to_string();
    let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let trigger = body.get("trigger").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let behavior = body.get("behavior").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let injection_mode = body.get("injection_mode").and_then(|v| v.as_str()).unwrap_or("system").to_string();
    let agent_scope = body.get("agent_scope").and_then(|v| v.as_str()).map(|s| s.to_string());
    let knowledge_item_id = body.get("knowledge_item_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    let tool_calls: Option<Value> = body.get("tool_calls").cloned();
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO agent_skills (skill_id, account_id, knowledge_item_id, title, trigger, behavior, tool_calls, is_active, trigger_count, injection_mode, agent_scope, created_at) VALUES ($sid, $aid, $kiid, $title, $trigger, $behavior, $tc, true, 0, $imode, $scope, $now)")
        .bind(("sid", skill_id.clone()))
        .bind(("aid", account_id))
        .bind(("kiid", knowledge_item_id))
        .bind(("title", title))
        .bind(("trigger", trigger))
        .bind(("behavior", behavior))
        .bind(("tc", tool_calls))
        .bind(("imode", injection_mode))
        .bind(("scope", agent_scope))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "skill_id": skill_id })))
}

async fn get_agent_skill(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(skill_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let mut resp = state
        .db
        .query("SELECT *, record::id(id) AS id FROM agent_skills WHERE skill_id = $sid AND account_id = $aid LIMIT 1")
        .bind(("sid", skill_id))
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match rows.into_iter().next() {
        Some(row) => Ok(Json(row)),
        None => Err((StatusCode::NOT_FOUND, "Agent skill not found".to_string())),
    }
}

async fn update_agent_skill(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(skill_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let now = Utc::now().timestamp();
    let mut set_parts = vec!["updated_at = $now".to_string()];

    if body.get("title").is_some()          { set_parts.push("title = $title".to_string()); }
    if body.get("trigger").is_some()        { set_parts.push("trigger = $trigger".to_string()); }
    if body.get("behavior").is_some()       { set_parts.push("behavior = $behavior".to_string()); }
    if body.get("is_active").is_some()      { set_parts.push("is_active = $active".to_string()); }
    if body.get("tool_calls").is_some()     { set_parts.push("tool_calls = $tool_calls".to_string()); }
    if body.get("injection_mode").is_some() { set_parts.push("injection_mode = $injection_mode".to_string()); }
    if body.get("agent_scope").is_some()    { set_parts.push("agent_scope = $agent_scope".to_string()); }

    let query = format!("UPDATE agent_skills SET {} WHERE skill_id = $sid AND account_id = $aid", set_parts.join(", "));
    let mut qb = state.db.query(&query)
        .bind(("now", now))
        .bind(("sid", skill_id))
        .bind(("aid", account_id));

    if let Some(v) = body.get("title").and_then(|v| v.as_str())          { qb = qb.bind(("title", v.to_string())); }
    if let Some(v) = body.get("trigger").and_then(|v| v.as_str())        { qb = qb.bind(("trigger", v.to_string())); }
    if let Some(v) = body.get("behavior").and_then(|v| v.as_str())       { qb = qb.bind(("behavior", v.to_string())); }
    if let Some(v) = body.get("is_active").and_then(|v| v.as_bool())     { qb = qb.bind(("active", v)); }
    if let Some(v) = body.get("tool_calls")                               { qb = qb.bind(("tool_calls", v.clone())); }
    if let Some(v) = body.get("injection_mode").and_then(|v| v.as_str()) { qb = qb.bind(("injection_mode", v.to_string())); }
    if let Some(v) = body.get("agent_scope").and_then(|v| v.as_str())    { qb = qb.bind(("agent_scope", v.to_string())); }

    qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete_agent_skill(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(skill_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    state
        .db
        .query("DELETE FROM agent_skills WHERE skill_id = $sid AND account_id = $aid")
        .bind(("sid", skill_id))
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn toggle_agent_skill(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(skill_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let is_active = body.get("is_active").and_then(|v| v.as_bool()).unwrap_or(true);
    state
        .db
        .query("UPDATE agent_skills SET is_active = $active WHERE skill_id = $sid AND account_id = $aid")
        .bind(("active", is_active))
        .bind(("sid", skill_id))
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn bump_skill_trigger(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(skill_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let now = Utc::now().timestamp();
    state
        .db
        .query("UPDATE agent_skills SET trigger_count = (trigger_count OR 0) + 1, last_triggered_at = $now WHERE skill_id = $sid AND account_id = $aid")
        .bind(("now", now))
        .bind(("sid", skill_id))
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

/// POST /skills/seed-builtins
/// 幂等重建此 account 的所有內建 skills 與 agent definitions。
async fn seed_builtins(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    crate::seeds::seed_builtins(&state.db, &account_id).await;
    Ok(Json(json!({ "ok": true, "account_id": account_id })))
}

// ── Agent Tools ──────────────────────────────────────────────────────────────

async fn list_agent_tools(
    State(state): State<ApiState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT *, record::id(id) AS id FROM agent_tools ORDER BY is_builtin DESC, created_at DESC")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn create_agent_tool(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let tool_id = Uuid::new_v4().to_string();
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let description = body.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let schema_json = body.get("schema_json").and_then(|v| v.as_str()).map(|s| s.to_string());
    let is_builtin = body.get("is_builtin").and_then(|v| v.as_bool()).unwrap_or(false);
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO agent_tools (tool_id, name, description, schema_json, is_active, is_builtin, created_at) VALUES ($tid, $name, $desc, $schema, true, $builtin, $now)")
        .bind(("tid", tool_id.clone()))
        .bind(("name", name))
        .bind(("desc", description))
        .bind(("schema", schema_json))
        .bind(("builtin", is_builtin))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "tool_id": tool_id })))
}

async fn update_agent_tool(
    State(state): State<ApiState>,
    Path(tool_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let description = body.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let is_active = body.get("is_active").and_then(|v| v.as_bool()).unwrap_or(true);

    state
        .db
        .query("UPDATE agent_tools SET name = $name, description = $desc, is_active = $active WHERE tool_id = $tid")
        .bind(("name", name))
        .bind(("desc", description))
        .bind(("active", is_active))
        .bind(("tid", tool_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn delete_agent_tool(
    State(state): State<ApiState>,
    Path(tool_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state
        .db
        .query("DELETE FROM agent_tools WHERE tool_id = $tid")
        .bind(("tid", tool_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

// ── Interactive agent endpoints ───────────────────────────────────────────────

/// POST /vaults/:vid/agent/run
/// Body: { session_id, messages: [{role, content}], tool_names: [], vault_path }
/// Spawns run_interactive_agent in background; immediately returns { session_id }.
async fn agent_run(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(vault_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let session_id = body["session_id"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let messages: Vec<serde_json::Value> = body["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let tool_names: Vec<String> = body["tool_names"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
        .unwrap_or_default();
    let vault_path = body["vault_path"].as_str().unwrap_or("").to_string();

    tokio::spawn(crate::service_agent::run_interactive_agent(
        state,
        session_id.clone(),
        messages,
        tool_names,
        vault_id,
        account_id,
        vault_path,
    ));

    Ok(Json(json!({ "session_id": session_id })))
}

/// POST /vaults/:vid/agent/cancel
/// Body: { session_id }
/// Sets the cancel flag for the given session.
async fn agent_cancel(
    State(state): State<ApiState>,
    Path(_vault_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let session_id = body["session_id"].as_str().unwrap_or("");
    let sessions = state.daemon.agent_sessions.lock().await;
    if let Some(sess) = sessions.get(session_id) {
        sess.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(Json(json!({ "ok": true })))
}

/// POST /vaults/:vid/agent/confirm
/// Body: { session_id, approved: bool }
/// Resolves the pending write-confirm oneshot for the given session.
async fn agent_confirm(
    State(state): State<ApiState>,
    Path(_vault_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let session_id = body["session_id"].as_str().unwrap_or("");
    let approved = body["approved"].as_bool().unwrap_or(false);
    let mut sessions = state.daemon.agent_sessions.lock().await;
    if let Some(sess) = sessions.get_mut(session_id) {
        if let Some(tx) = sess.confirm_tx.take() {
            let _ = tx.send(approved);
        }
    }
    Ok(Json(json!({ "ok": true })))
}
