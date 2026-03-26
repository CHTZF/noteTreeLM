use crate::{api_client::{daemon_get, daemon_post, daemon_patch}, error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Serialize, Deserialize)]
pub struct PatternRow {
    pub signature: String,
    pub score: f64,
    pub trigger_count: i64,
    pub speak_count: i64,
    pub deprecated: bool,
    pub semantic_intent: Option<String>,
}

#[tauri::command]
pub async fn save_pattern(
    state: State<'_, AppState>,
    vault_id: String,
    signature: String,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/activity-patterns", urlencoding::encode(&vault_id)),
        &serde_json::json!({"signature": signature}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Database(e))
}

#[tauri::command]
pub async fn update_pattern_score(
    state: State<'_, AppState>,
    vault_id: String,
    signature: String,
    spoke: bool,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/activity-patterns/score", urlencoding::encode(&vault_id)),
        &serde_json::json!({"signature": signature, "spoke": spoke}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Database(e))
}

#[tauri::command]
pub async fn list_patterns(
    state: State<'_, AppState>,
    vault_id: String,
    min_score: Option<f64>,
) -> Result<Vec<PatternRow>, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let min = min_score.unwrap_or(0.0);
    let rows: Vec<serde_json::Value> = daemon_get(
        &state.http_client,
        &format!("/vaults/{}/activity-patterns?min_score={}", urlencoding::encode(&vault_id), min),
        tok,
    ).await.unwrap_or_default();
    Ok(rows.into_iter().map(|r| PatternRow {
        signature: r["signature"].as_str().unwrap_or("").to_string(),
        score: r["score"].as_f64().unwrap_or(0.0),
        trigger_count: r["trigger_count"].as_i64().unwrap_or(0),
        speak_count: r["speak_count"].as_i64().unwrap_or(0),
        deprecated: r["deprecated"].as_bool().unwrap_or(false),
        semantic_intent: r["semantic_intent"].as_str().map(|s| s.to_string()),
    }).collect())
}

#[tauri::command]
pub async fn decay_patterns(
    state: State<'_, AppState>,
    vault_id: String,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/activity-patterns/decay", urlencoding::encode(&vault_id)),
        &serde_json::json!({}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Database(e))
}

#[tauri::command]
pub async fn set_pattern_intent(
    state: State<'_, AppState>,
    vault_id: String,
    signature: String,
    intent: String,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    daemon_patch::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/activity-patterns/intent", urlencoding::encode(&vault_id)),
        &serde_json::json!({"signature": signature, "semantic_intent": intent}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Database(e))
}
