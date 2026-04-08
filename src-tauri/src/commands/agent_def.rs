#![allow(dead_code)]
use crate::{
    api_client::{daemon_get, daemon_post, daemon_put, daemon_patch, daemon_delete},
    error::AppError,
    state::AppState,
};
use serde::{Deserialize, Serialize};
use tauri::State;

// ── Data Structures ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentDefinition {
    pub def_id: String,
    pub vault_id: String,
    pub name: String,
    pub description: String,
    pub kind: String,           // 'main' | 'sub'
    pub skill_ids: Vec<String>,
    pub tool_names: Vec<String>,
    pub system_prompt: String,
    pub max_rounds: i64,
    pub is_active: bool,
    pub is_builtin: bool,
    pub trigger: String,
    pub created_at: i64,        // ms timestamp
    pub status: String,         // 'active' | 'sleep'
    pub slept_at: Option<i64>,  // ms timestamp, set when entering sleep
    pub use_count: i64,
    pub last_used_at: Option<i64>,
}

// ── Helper to parse AgentDefinition from JSON value ───────────────────────────

pub(crate) fn agent_from_json(r: &serde_json::Value) -> AgentDefinition {
    AgentDefinition {
        def_id: r["def_id"].as_str().unwrap_or("").to_string(),
        vault_id: r["vault_id"].as_str().unwrap_or("").to_string(),
        name: r["name"].as_str().unwrap_or("").to_string(),
        description: r["description"].as_str().unwrap_or("").to_string(),
        kind: r["kind"].as_str().unwrap_or("sub").to_string(),
        skill_ids: r["skill_ids"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default(),
        tool_names: r["tool_names"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default(),
        system_prompt: r["system_prompt"].as_str().unwrap_or("").to_string(),
        max_rounds: r["max_rounds"].as_i64().unwrap_or(5),
        is_active: r["is_active"].as_bool().unwrap_or(true),
        is_builtin: r["is_builtin"].as_bool().unwrap_or(false),
        trigger: r["trigger"].as_str().unwrap_or("").to_string(),
        created_at: r["created_at"].as_i64().unwrap_or(0),
        status: r["status"].as_str().map(|s| if s.is_empty() { "active" } else { s }).unwrap_or("active").to_string(),
        slept_at: r["slept_at"].as_i64(),
        use_count: r["use_count"].as_i64().unwrap_or(0),
        last_used_at: r["last_used_at"].as_i64(),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn valid_kind(s: &str) -> &str {
    match s { "main" | "sub" => s, _ => "sub" }
}

// ── Public Tauri Commands (daemon API) ────────────────────────────────────────

/// 列出 vault 所有 agent definitions（builtin + 使用者自訂）
#[tauri::command]
pub async fn list_agent_definitions(
    state: State<'_, AppState>,
) -> Result<Vec<AgentDefinition>, AppError> {
    let vault_id = state.get_vault_id().await?;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    let rows: Vec<serde_json::Value> = daemon_get(
        &state.http_client,
        &format!("/vaults/{}/agents", urlencoding::encode(&vault_id)),
        tok,
    ).await.unwrap_or_default();

    Ok(rows.iter().map(agent_from_json).collect())
}

/// 建立使用者自訂的 agent definition
#[tauri::command]
pub async fn save_agent_definition(
    state: State<'_, AppState>,
    name: String,
    description: String,
    kind: Option<String>,
    skill_ids: Vec<String>,
    tool_names: Vec<String>,
    system_prompt: Option<String>,
    max_rounds: Option<i64>,
    trigger: Option<String>,
) -> Result<AgentDefinition, AppError> {
    let vault_id = state.get_vault_id().await?;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    let kind_str = valid_kind(kind.as_deref().unwrap_or("sub")).to_string();
    let prompt = system_prompt.unwrap_or_default();
    let rounds = max_rounds.unwrap_or(5).max(1).min(20);
    let trigger_str = trigger.unwrap_or_default();

    let r: serde_json::Value = daemon_post(
        &state.http_client,
        &format!("/vaults/{}/agents", urlencoding::encode(&vault_id)),
        &serde_json::json!({
            "name": name,
            "description": description,
            "kind": kind_str,
            "skill_ids": skill_ids,
            "tool_names": tool_names,
            "system_prompt": prompt,
            "max_rounds": rounds,
            "trigger": trigger_str,
        }),
        tok,
    ).await.map_err(|e| AppError::Database(e))?;

    Ok(agent_from_json(&r))
}

/// 更新 agent definition
#[tauri::command]
pub async fn update_agent_definition(
    state: State<'_, AppState>,
    def_id: String,
    name: String,
    description: String,
    skill_ids: Vec<String>,
    tool_names: Vec<String>,
    system_prompt: Option<String>,
    max_rounds: Option<i64>,
    trigger: Option<String>,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let vault_id = state.get_vault_id().await?;

    let prompt = system_prompt.unwrap_or_default();
    let rounds = max_rounds.unwrap_or(5).max(1).min(20);
    let trigger_str = trigger.unwrap_or_default();

    daemon_put::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/agents/{}", urlencoding::encode(&vault_id), urlencoding::encode(&def_id)),
        &serde_json::json!({
            "name": name,
            "description": description,
            "skill_ids": skill_ids,
            "tool_names": tool_names,
            "system_prompt": prompt,
            "max_rounds": rounds,
            "trigger": trigger_str,
        }),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Database(e))
}

/// 刪除使用者自訂的 agent definition（is_builtin = true 的不能刪）
#[tauri::command]
pub async fn delete_agent_definition(
    state: State<'_, AppState>,
    def_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    daemon_delete::<serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/agents/{}", urlencoding::encode(&vault_id), urlencoding::encode(&def_id)),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Database(e))
}

/// 啟用或停用一個 agent definition
#[tauri::command]
pub async fn toggle_agent_definition(
    state: State<'_, AppState>,
    def_id: String,
    is_active: bool,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    daemon_patch::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/agents/{}/toggle", urlencoding::encode(&vault_id), urlencoding::encode(&def_id)),
        &serde_json::json!({"is_active": is_active}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Database(e))
}

/// 手動 wake 一個 sleep 中的 agent（前端 UI 呼叫）
#[tauri::command]
pub async fn wake_agent_definition(
    state: State<'_, AppState>,
    def_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    daemon_patch::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/agents/{}/wake", urlencoding::encode(&vault_id), urlencoding::encode(&def_id)),
        &serde_json::json!({}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Database(e))
}

/// 列出本次 session 中所有 ephemeral agents（供 AgentsPage UI 顯示）
#[tauri::command]
pub async fn list_ephemeral_agents(
    state: State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, AppError> {
    let vault_id = state.get_vault_id().await.unwrap_or_default();
    let auth_token = state.get_auth_token().await;
    let tok = if auth_token.is_empty() { None } else { Some(auth_token.as_str()) };
    let result = crate::api_client::daemon_get::<Vec<serde_json::Value>>(
        &state.http_client,
        &format!("/vaults/{}/agents/ephemeral", urlencoding::encode(&vault_id)),
        tok,
    ).await.unwrap_or_default();
    Ok(result)
}

/// 清除本次 session 的 ephemeral agents 記錄
#[tauri::command]
pub async fn clear_ephemeral_agents(
    state: State<'_, AppState>,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await.unwrap_or_default();
    let auth_token = state.get_auth_token().await;
    let tok = if auth_token.is_empty() { None } else { Some(auth_token.as_str()) };
    let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/agents/ephemeral/clear", urlencoding::encode(&vault_id)),
        &serde_json::json!({}),
        tok,
    ).await;
    Ok(())
}

// ── Internal helpers (Phase 8: use daemon API) ────────────────────────────────

/// 從 daemon 取得單一 definition（供 SystemAgentService 路由用）
pub async fn get_agent_definition_by_id(
    client: &reqwest::Client,
    tok: Option<&str>,
    vault_id: &str,
    def_id: &str,
) -> Option<AgentDefinition> {
    let r: serde_json::Value = crate::api_client::daemon_get(
        client,
        &format!("/vaults/{}/agents/{}", urlencoding::encode(vault_id), urlencoding::encode(def_id)),
        tok,
    ).await.ok()?;
    if r.is_null() || r["def_id"].as_str().is_none() { return None; }
    Some(agent_from_json(&r))
}

/// 按 name（模糊）或 def_id 找最符合的 definition（供 SystemAgentService 路由用）
pub async fn find_agent_definition(
    client: &reqwest::Client,
    tok: Option<&str>,
    vault_id: &str,
    target: &str,
) -> Option<AgentDefinition> {
    let rows: Vec<serde_json::Value> = crate::api_client::daemon_get(
        client,
        &format!("/vaults/{}/agents", urlencoding::encode(vault_id)),
        tok,
    ).await.unwrap_or_default();

    // Try exact def_id match first
    if let Some(r) = rows.iter().find(|r| r["def_id"].as_str() == Some(target)) {
        return Some(agent_from_json(r));
    }
    // Try name/description contains match
    let target_lower = target.to_lowercase();
    rows.iter()
        .filter(|r| r["is_active"].as_bool().unwrap_or(true))
        .find(|r| {
            let name = r["name"].as_str().unwrap_or("").to_lowercase();
            let desc = r["description"].as_str().unwrap_or("").to_lowercase();
            name.contains(&target_lower) || target_lower.contains(&name) || desc.contains(&target_lower)
        })
        .map(agent_from_json)
}

/// 找到與 user_embedding cosine similarity 最高的 agent definition
pub async fn find_matching_agent_definition(
    client: &reqwest::Client,
    tok: Option<&str>,
    vault_id: &str,
    user_embedding: &[f32],
    threshold: f32,
    active_only: bool,
) -> Option<AgentDefinition> {
    let rows: Vec<serde_json::Value> = crate::api_client::daemon_get(
        client,
        &format!("/vaults/{}/agents", urlencoding::encode(vault_id)),
        tok,
    ).await.unwrap_or_default();

    let mut best_score = threshold;
    let mut best: Option<&serde_json::Value> = None;

    for row in &rows {
        if !row["is_active"].as_bool().unwrap_or(true) { continue; }
        if active_only {
            let status = row["status"].as_str().unwrap_or("active");
            if status != "active" && !status.is_empty() { continue; }
        }
        let emb: Vec<f32> = row["trigger_embedding"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect())
            .unwrap_or_default();
        if emb.is_empty() { continue; }
        let score = cosine_similarity(user_embedding, &emb);
        if score > best_score {
            best_score = score;
            best = Some(row);
        }
    }
    best.map(agent_from_json)
}

/// 記錄 agent 被呼叫（use_count+1, last_used_at=now, 若在 sleep 則自動 wake）
pub async fn record_agent_usage(client: &reqwest::Client, tok: Option<&str>, vault_id: &str, def_id: &str) {
    let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
        client,
        &format!("/vaults/{}/agents/{}/usage", urlencoding::encode(vault_id), urlencoding::encode(def_id)),
        &serde_json::json!({}),
        tok,
    ).await;
}

/// 生命週期管理：進 sleep（7天）、刪除（sleep 23天）
pub async fn check_agent_lifecycle(client: &reqwest::Client, tok: Option<&str>, vault_id: &str) {
    let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
        client,
        &format!("/vaults/{}/agents/lifecycle", urlencoding::encode(vault_id)),
        &serde_json::json!({}),
        tok,
    ).await;
}

/// 種子內建 agent definitions（no-op: daemon manages seeds）
pub async fn seed_agent_definitions() {}

/// 種子內建 agent tools（no-op: daemon manages seeds）
pub async fn seed_agent_tools() {}

/// 種子內建 agent skills（no-op: daemon manages seeds）
pub async fn seed_agent_skills() {}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 { 0.0 } else { dot / (norm_a * norm_b) }
}

/// 將 use_ask 加入指定技能的 trigger 欄位，並重新計算 trigger_embedding。
#[tauri::command]
pub async fn add_skill_trigger(
    skill_id: String,
    use_ask: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let vault_id = state.get_vault_id().await.map_err(|e| e.to_string())?;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    // GET current skill trigger from daemon
    let current_skill: serde_json::Value = daemon_get(
        &state.http_client,
        &format!("/vaults/{}/skills/{}", urlencoding::encode(&vault_id), urlencoding::encode(&skill_id)),
        tok,
    ).await.unwrap_or_else(|_| serde_json::json!({}));

    let current_trigger = current_skill["trigger"].as_str().unwrap_or("").to_string();
    let new_trigger = if current_trigger.is_empty() {
        use_ask.clone()
    } else {
        format!("{}、{}", current_trigger, use_ask)
    };

    let mut update_body = serde_json::json!({"trigger": new_trigger});

    let new_embedding = crate::commands::server::get_embedding_via_service(state.inner(), &new_trigger).await;
    if !new_embedding.is_empty() {
        update_body["trigger_embedding"] = serde_json::json!(new_embedding);
    }

    daemon_put::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/skills/{}", urlencoding::encode(&vault_id), urlencoding::encode(&skill_id)),
        &update_body,
        tok,
    ).await.map(|_| ()).map_err(|e| e)
}
