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
    let mut resp = state.db
        .query("SELECT *, record::id(id) AS id FROM agent_skills WHERE account_id = $aid ORDER BY created_at DESC")
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
    let skill_id = Uuid::new_v4().to_string();
    let title          = body["title"].as_str().unwrap_or("").to_string();
    let trigger        = body["trigger"].as_str().unwrap_or("").to_string();
    let behavior       = body["behavior"].as_str().unwrap_or("").to_string();
    let injection_mode = body["injection_mode"].as_str().unwrap_or("system").to_string();
    let agent_scope    = body["agent_scope"].as_str().map(|s| s.to_string());
    let knowledge_item_id = body["knowledge_item_id"].as_str().map(|s| s.to_string());
    let tool_calls: Option<Value> = body.get("tool_calls").cloned();
    let now = Utc::now().timestamp();

    state.db
        .query("INSERT INTO agent_skills (skill_id, account_id, knowledge_item_id, title, trigger, behavior, tool_calls, is_active, trigger_count, injection_mode, agent_scope, created_at) VALUES ($sid, $aid, $kiid, $title, $trigger, $behavior, $tc, true, 0, $imode, $scope, $now)")
        .bind(("sid", skill_id.clone())).bind(("aid", account_id.clone()))
        .bind(("kiid", knowledge_item_id)).bind(("title", title))
        .bind(("trigger", trigger)).bind(("behavior", behavior))
        .bind(("tc", tool_calls)).bind(("imode", injection_mode))
        .bind(("scope", agent_scope)).bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let embed_url = state.daemon.embedding_url.read().await.clone();
    tokio::spawn(async move {
        crate::db::seeds::embed_skills_for_account(&state.db, &account_id, &embed_url).await;
    });

    Ok(Json(json!({ "skill_id": skill_id })))
}

pub async fn get_one(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(skill_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let mut resp = state.db
        .query("SELECT *, record::id(id) AS id FROM agent_skills WHERE skill_id = $sid AND account_id = $aid LIMIT 1")
        .bind(("sid", skill_id)).bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Value> = resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    rows.into_iter().next()
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "Agent skill not found".to_string()))
}

pub async fn update(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(skill_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let now = Utc::now().timestamp();
    let mut set_parts = vec!["updated_at = $now".to_string()];
    let semantic_changed = body.get("title").is_some()
        || body.get("trigger").is_some()
        || body.get("behavior").is_some();

    if body.get("title").is_some()          { set_parts.push("title = $title".to_string()); }
    if body.get("trigger").is_some()        { set_parts.push("trigger = $trigger".to_string()); }
    if body.get("behavior").is_some()       { set_parts.push("behavior = $behavior".to_string()); }
    if body.get("is_active").is_some()      { set_parts.push("is_active = $active".to_string()); }
    if body.get("tool_calls").is_some()       { set_parts.push("tool_calls = $tool_calls".to_string()); }
    if body.get("injection_mode").is_some()   { set_parts.push("injection_mode = $injection_mode".to_string()); }
    if body.get("agent_scope").is_some()      { set_parts.push("agent_scope = $agent_scope".to_string()); }
    if body.get("need_tool_chain").is_some()  { set_parts.push("need_tool_chain = $need_tool_chain".to_string()); }
    if body.get("tool_chain_order").is_some() { set_parts.push("tool_chain_order = $tool_chain_order".to_string()); }
    if semantic_changed                     { set_parts.push("embedding = NONE".to_string()); }

    let query = format!(
        "UPDATE agent_skills SET {} WHERE skill_id = $sid AND account_id = $aid",
        set_parts.join(", ")
    );
    let account_id_for_embed = account_id.clone();
    let mut qb = state.db.query(&query)
        .bind(("now", now)).bind(("sid", skill_id)).bind(("aid", account_id));

    if let Some(v) = body["title"].as_str()          { qb = qb.bind(("title", v.to_string())); }
    if let Some(v) = body["trigger"].as_str()        { qb = qb.bind(("trigger", v.to_string())); }
    if let Some(v) = body["behavior"].as_str()       { qb = qb.bind(("behavior", v.to_string())); }
    if let Some(v) = body["is_active"].as_bool()     { qb = qb.bind(("active", v)); }
    if let Some(v) = body.get("tool_calls")           { qb = qb.bind(("tool_calls", v.clone())); }
    if let Some(v) = body["injection_mode"].as_str()  { qb = qb.bind(("injection_mode", v.to_string())); }
    if let Some(v) = body["agent_scope"].as_str()     { qb = qb.bind(("agent_scope", v.to_string())); }
    if let Some(v) = body["need_tool_chain"].as_bool(){ qb = qb.bind(("need_tool_chain", v)); }
    if let Some(v) = body.get("tool_chain_order")     { qb = qb.bind(("tool_chain_order", v.clone())); }

    qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if semantic_changed {
        let embed_url = state.daemon.embedding_url.read().await.clone();
        tokio::spawn(async move {
            crate::db::seeds::embed_skills_for_account(&state.db, &account_id_for_embed, &embed_url).await;
        });
    }

    Ok(Json(json!({ "ok": true })))
}

pub async fn delete(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(skill_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    state.db
        .query("DELETE FROM agent_skills WHERE skill_id = $sid AND account_id = $aid")
        .bind(("sid", skill_id)).bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn toggle(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(skill_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let is_active = body["is_active"].as_bool().unwrap_or(true);
    state.db
        .query("UPDATE agent_skills SET is_active = $active WHERE skill_id = $sid AND account_id = $aid")
        .bind(("active", is_active)).bind(("sid", skill_id)).bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn bump_trigger(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(skill_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let now = Utc::now().timestamp();
    state.db
        .query("UPDATE agent_skills SET trigger_count = (trigger_count OR 0) + 1, last_triggered_at = $now WHERE skill_id = $sid AND account_id = $aid")
        .bind(("now", now)).bind(("sid", skill_id)).bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

/// POST /skills/seed-builtins — 幂等重建此 account 的所有內建 skills 與 agent definitions。
pub async fn seed_builtins(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let embed_url = state.daemon.embedding_url.read().await.clone();
    crate::db::seeds::seed_builtins(&state.db, &account_id).await;
    crate::db::seeds::embed_skills_for_account(&state.db, &account_id, &embed_url).await;
    Ok(Json(json!({ "ok": true, "account_id": account_id })))
}
