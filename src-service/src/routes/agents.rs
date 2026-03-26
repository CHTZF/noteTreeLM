use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/vaults/:vault_id/agents",
            get(list_agent_definitions).post(create_agent_definition),
        )
        .route(
            "/vaults/:vault_id/agents/:def_id",
            get(get_agent_definition).put(update_agent_definition).delete(delete_agent_definition),
        )
        .route(
            "/vaults/:vault_id/skills",
            get(list_agent_skills).post(create_agent_skill),
        )
        .route(
            "/vaults/:vault_id/skills/:skill_id",
            get(get_agent_skill).put(update_agent_skill).delete(delete_agent_skill),
        )
        .route(
            "/vaults/:vault_id/skills/:skill_id/toggle",
            patch(toggle_agent_skill),
        )
        .route("/agent-tools", get(list_agent_tools).post(create_agent_tool))
        .route(
            "/agent-tools/:tool_id",
            put(update_agent_tool).delete(delete_agent_tool),
        )
        .route(
            "/vaults/:vault_id/agents/:def_id/usage",
            post(record_agent_usage),
        )
        .route(
            "/vaults/:vault_id/agents/:def_id/wake",
            post(wake_agent),
        )
        .route(
            "/vaults/:vault_id/agents/lifecycle",
            post(run_agent_lifecycle),
        )
        .route(
            "/vaults/:vault_id/agents/:def_id/toggle",
            patch(toggle_agent),
        )
        .route(
            "/vaults/:vault_id/seed-builtins",
            post(seed_builtins),
        )
        .route(
            "/vaults/:vault_id/skills/:skill_id/trigger",
            post(bump_skill_trigger),
        )
}

// ── Agent Definitions ────────────────────────────────────────────────────────

async fn list_agent_definitions(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM agent_definitions WHERE vault_id = $vid OR is_builtin = true ORDER BY created_at DESC")
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn create_agent_definition(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let def_id = Uuid::new_v4().to_string();
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let description = body.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let kind = body.get("kind").and_then(|v| v.as_str()).unwrap_or("chat").to_string();
    let system_prompt = body.get("system_prompt").and_then(|v| v.as_str()).map(|s| s.to_string());
    let max_rounds = body.get("max_rounds").and_then(|v| v.as_i64()).unwrap_or(10);
    let skill_ids = body.get("skill_ids").map(|v| v.to_string());
    let tool_names = body.get("tool_names").map(|v| v.to_string());
    let trigger = body.get("trigger").and_then(|v| v.as_str()).map(|s| s.to_string());
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO agent_definitions (def_id, vault_id, name, description, kind, skill_ids, tool_names, system_prompt, max_rounds, is_active, is_builtin, trigger, created_at) VALUES ($did, $vid, $name, $desc, $kind, $skills, $tools, $prompt, $rounds, true, false, $trigger, $now)")
        .bind(("did", def_id.clone()))
        .bind(("vid", vault_id))
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
    Path((vault_id, def_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM agent_definitions WHERE def_id = $did AND (vault_id = $vid OR is_builtin = true) LIMIT 1")
        .bind(("did", def_id))
        .bind(("vid", vault_id))
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
    Path((vault_id, def_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    let mut set_parts = vec!["created_at = created_at".to_string()]; // no-op anchor

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
    let query = format!("UPDATE agent_definitions SET {set_clause}, created_at = created_at WHERE def_id = $did AND vault_id = $vid");

    let mut qb = state.db.query(&query).bind(("vid", vault_id));
    if let Some(v) = body.get("name").and_then(|v| v.as_str()) {
        qb = qb.bind(("name", v.to_string()));
    }
    if let Some(v) = body.get("description").and_then(|v| v.as_str()) {
        qb = qb.bind(("description", v.to_string()));
    }
    if let Some(v) = body.get("kind").and_then(|v| v.as_str()) {
        qb = qb.bind(("kind", v.to_string()));
    }
    if let Some(v) = body.get("system_prompt").and_then(|v| v.as_str()) {
        qb = qb.bind(("system_prompt", v.to_string()));
    }
    if let Some(v) = body.get("trigger").and_then(|v| v.as_str()) {
        qb = qb.bind(("trigger", v.to_string()));
    }
    let max_rounds = body.get("max_rounds").and_then(|v| v.as_i64()).unwrap_or(10);
    qb = qb.bind(("max_rounds", max_rounds));
    qb = qb.bind(("did", def_id));
    let _ = now; // suppress unused warning

    qb.await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn delete_agent_definition(
    State(state): State<ApiState>,
    Path((vault_id, def_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state
        .db
        .query("DELETE FROM agent_definitions WHERE def_id = $did AND vault_id = $vid")
        .bind(("did", def_id))
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

// ── Agent Skills ─────────────────────────────────────────────────────────────

async fn list_agent_skills(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM agent_skills WHERE vault_id = $vid ORDER BY created_at DESC")
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn create_agent_skill(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let skill_id = Uuid::new_v4().to_string();
    let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let trigger = body.get("trigger").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let behavior = body.get("behavior").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let injection_mode = body.get("injection_mode").and_then(|v| v.as_str()).unwrap_or("system").to_string();
    let agent_scope = body.get("agent_scope").and_then(|v| v.as_str()).map(|s| s.to_string());
    let knowledge_item_id = body.get("knowledge_item_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    // tool_calls 儲存為 native JSON array（非字串），讓 Tauri 端 as_array() 可直接解析
    let tool_calls: Option<Value> = body.get("tool_calls").cloned();
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO agent_skills (skill_id, vault_id, knowledge_item_id, title, trigger, behavior, tool_calls, is_active, trigger_count, injection_mode, agent_scope, created_at) VALUES ($sid, $vid, $kiid, $title, $trigger, $behavior, $tc, true, 0, $imode, $scope, $now)")
        .bind(("sid", skill_id.clone()))
        .bind(("vid", vault_id))
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
    Path((vault_id, skill_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM agent_skills WHERE skill_id = $sid AND vault_id = $vid LIMIT 1")
        .bind(("sid", skill_id))
        .bind(("vid", vault_id))
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
    Path((vault_id, skill_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    let mut set_parts = vec!["updated_at = $now".to_string()];

    if body.get("title").is_some()          { set_parts.push("title = $title".to_string()); }
    if body.get("trigger").is_some()        { set_parts.push("trigger = $trigger".to_string()); }
    if body.get("behavior").is_some()       { set_parts.push("behavior = $behavior".to_string()); }
    if body.get("is_active").is_some()      { set_parts.push("is_active = $active".to_string()); }
    if body.get("tool_calls").is_some()     { set_parts.push("tool_calls = $tool_calls".to_string()); }
    if body.get("injection_mode").is_some() { set_parts.push("injection_mode = $injection_mode".to_string()); }
    if body.get("agent_scope").is_some()    { set_parts.push("agent_scope = $agent_scope".to_string()); }

    let query = format!("UPDATE agent_skills SET {} WHERE skill_id = $sid AND vault_id = $vid", set_parts.join(", "));
    let mut qb = state.db.query(&query)
        .bind(("now", now))
        .bind(("sid", skill_id))
        .bind(("vid", vault_id));

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
    Path((vault_id, skill_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state
        .db
        .query("DELETE FROM agent_skills WHERE skill_id = $sid AND vault_id = $vid")
        .bind(("sid", skill_id))
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn toggle_agent_skill(
    State(state): State<ApiState>,
    Path((vault_id, skill_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let is_active = body.get("is_active").and_then(|v| v.as_bool()).unwrap_or(true);
    state
        .db
        .query("UPDATE agent_skills SET is_active = $active WHERE skill_id = $sid AND vault_id = $vid")
        .bind(("active", is_active))
        .bind(("sid", skill_id))
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

// ── Agent Tools ──────────────────────────────────────────────────────────────

async fn list_agent_tools(
    State(state): State<ApiState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM agent_tools ORDER BY is_builtin DESC, created_at DESC")
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

async fn record_agent_usage(
    State(state): State<ApiState>,
    Path((vault_id, def_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    state
        .db
        .query("UPDATE agent_definitions SET use_count = (use_count OR 0) + 1, last_used_at = $now, status = 'active', slept_at = NONE WHERE def_id = $did AND (vault_id = $vid OR is_builtin = true)")
        .bind(("now", now))
        .bind(("did", def_id))
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn wake_agent(
    State(state): State<ApiState>,
    Path((vault_id, def_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state
        .db
        .query("UPDATE agent_definitions SET status = 'active', slept_at = NONE WHERE def_id = $did AND vault_id = $vid AND is_builtin = false")
        .bind(("did", def_id))
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn run_agent_lifecycle(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    let sleep_cutoff = now - 7 * 86400;
    let delete_cutoff = now - 23 * 86400;

    // Delete agents that have been sleeping for > 23 days
    state
        .db
        .query("DELETE agent_definitions WHERE vault_id = $vid AND is_builtin = false AND status = 'sleep' AND slept_at != NONE AND slept_at < $dcutoff")
        .bind(("vid", vault_id.clone()))
        .bind(("dcutoff", delete_cutoff))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Put unused agents to sleep
    state
        .db
        .query("UPDATE agent_definitions SET status = 'sleep', slept_at = $now WHERE vault_id = $vid AND is_builtin = false AND status != 'sleep' AND ((last_used_at != NONE AND last_used_at < $scutoff) OR (last_used_at = NONE AND created_at < $scutoff))")
        .bind(("vid", vault_id))
        .bind(("now", now))
        .bind(("scutoff", sleep_cutoff))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn toggle_agent(
    State(state): State<ApiState>,
    Path((vault_id, def_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let is_active = body.get("is_active").and_then(|v| v.as_bool()).unwrap_or(true);
    state
        .db
        .query("UPDATE agent_definitions SET is_active = $active WHERE def_id = $did AND (vault_id = $vid OR is_builtin = true)")
        .bind(("active", is_active))
        .bind(("did", def_id))
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

/// POST /vaults/:vault_id/seed-builtins
/// 幂等重建此 vault 的所有內建 skills 與 agent definitions。
/// Tauri 端在每次啟動、開啟 vault 時呼叫。
async fn seed_builtins(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    crate::seeds::seed_builtins(&state.db, &vault_id).await;
    Ok(Json(json!({ "ok": true, "vault_id": vault_id })))
}

/// POST /vaults/:vault_id/skills/:skill_id/trigger
/// 累加 skill trigger_count，記錄最後觸發時間。
async fn bump_skill_trigger(
    State(state): State<ApiState>,
    Path((vault_id, skill_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    state
        .db
        .query("UPDATE agent_skills SET trigger_count = (trigger_count OR 0) + 1, last_triggered_at = $now WHERE skill_id = $sid AND vault_id = $vid")
        .bind(("now", now))
        .bind(("sid", skill_id))
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}
