use crate::{api_client::{daemon_get, daemon_post}, error::AppError, state::AppState};
use serde::Serialize;
use tauri::State;

// ─── Memory Query ──────────────────────────────────────────────────────────────

/// 查詢記憶筆記（供前端直接呼叫，非 agent 工具版）
#[derive(Debug, Serialize)]
pub struct MemoryResult {
    pub path: String,
    pub title: String,
    pub created_at: i64,
    pub snippet: String,
}

#[tauri::command]
pub async fn query_memory(
    state: State<'_, AppState>,
    keywords: Vec<String>,
    since: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<MemoryResult>, AppError> {
    let limit = limit.unwrap_or(10).min(50);
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let vault_id = state.get_vault_id().await?;

    let kw_param = keywords.join(",");
    let since_param = since.unwrap_or_default();
    let url = format!(
        "/vaults/{}/memory/query?keywords={}&since={}&limit={}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&kw_param),
        urlencoding::encode(&since_param),
        limit,
    );

    let rows: Vec<serde_json::Value> = daemon_get(
        &state.http_client,
        &url,
        tok,
    ).await.unwrap_or_default();

    Ok(rows.into_iter().map(|r| MemoryResult {
        path: r["path"].as_str().unwrap_or("").to_string(),
        title: r["title"].as_str().unwrap_or("").to_string(),
        created_at: r["created_at"].as_i64().unwrap_or(0),
        snippet: r["snippet"].as_str().unwrap_or("").to_string(),
    }).collect())
}

// ─── Response Ratings ─────────────────────────────────────────────────────────

#[tauri::command]
pub async fn rate_response(
    state: State<'_, AppState>,
    conversation_id: Option<String>,
    content_hash: String,
    rating: String,
) -> Result<(), AppError> {
    let vault_id = match state.get_vault_id().await {
        Ok(id) => id,
        Err(_) => return Ok(()),
    };
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/ratings", urlencoding::encode(&vault_id)),
        &serde_json::json!({
            "conversation_id": conversation_id,
            "content_hash": content_hash,
            "rating": rating,
        }),
        tok,
    ).await.map(|_| ()).map_err(AppError::Database)
}

#[tauri::command]
pub async fn get_conversation_ratings(
    state: State<'_, AppState>,
    conversation_id: String,
) -> Result<Vec<serde_json::Value>, AppError> {
    let vault_id = match state.get_vault_id().await {
        Ok(id) => id,
        Err(_) => return Ok(vec![]),
    };
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let rows: Vec<serde_json::Value> = daemon_get(
        &state.http_client,
        &format!(
            "/vaults/{}/ratings?conversation_id={}",
            urlencoding::encode(&vault_id),
            urlencoding::encode(&conversation_id),
        ),
        tok,
    ).await.unwrap_or_default();
    Ok(rows)
}
