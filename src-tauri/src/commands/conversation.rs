use serde::{Deserialize, Serialize};
use tauri::State;

use crate::error::AppError;
use crate::state::AppState;
use crate::api_client::{daemon_get, daemon_post, daemon_patch, daemon_delete};

// ── Response types ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ConversationSummary {
    pub id: String,
    pub mode: String,
    pub title: String,
    pub updated_at: i64,
    pub has_pending_plan: bool,
}

#[derive(Debug, Serialize)]
pub struct ConversationSnapshot {
    pub id: String,
    pub mode: String,
    pub title: String,
    pub messages_json: String,
    pub has_pending_plan: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

// ── Commands ───────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn create_conversation(
    state: State<'_, AppState>,
    username: String,
    mode: String,
    title: Option<String>,
) -> Result<String, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let vault_id = state.get_vault_uuid().await;
    let title = title.unwrap_or_default();

    #[derive(Deserialize)]
    struct Resp { id: String }
    let resp = daemon_post::<_, Resp>(
        &state.http_client,
        "/conversations",
        &serde_json::json!({"vault_id": vault_id, "account_id": username, "mode": mode, "title": title}),
        tok,
    ).await.map_err(|e| AppError::Database(e))?;
    Ok(resp.id)
}

#[tauri::command]
pub async fn list_conversations(
    state: State<'_, AppState>,
    username: String,
    mode: String,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<ConversationSummary>, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let vault_id = state.get_vault_uuid().await;
    let limit = limit.unwrap_or(50);
    let offset = offset.unwrap_or(0);

    let rows: Vec<serde_json::Value> = daemon_get(
        &state.http_client,
        &format!("/conversations?vault_id={}&mode={}&account_id={}&limit={}&offset={}",
            urlencoding::encode(&vault_id),
            urlencoding::encode(&mode),
            urlencoding::encode(&username),
            limit, offset),
        tok,
    ).await.unwrap_or_default();

    Ok(rows.into_iter().map(|r| ConversationSummary {
        id: r["id"].as_str().unwrap_or("").to_string(),
        mode: r["mode"].as_str().unwrap_or("").to_string(),
        title: r["title"].as_str().unwrap_or("").to_string(),
        updated_at: r["updated_at"].as_i64().unwrap_or(0),
        has_pending_plan: r["has_pending_plan"].as_bool().unwrap_or(false),
    }).collect())
}

#[tauri::command]
pub async fn get_conversation(
    state: State<'_, AppState>,
    id: String,
) -> Result<ConversationSnapshot, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    let r: serde_json::Value = daemon_get(
        &state.http_client,
        &format!("/conversations/{}", urlencoding::encode(&id)),
        tok,
    ).await.map_err(|e| AppError::Database(e))?;

    let messages_json = r["messages_json"].as_str().unwrap_or("[]");
    let display = filter_display_messages(messages_json);

    Ok(ConversationSnapshot {
        id: r["id"].as_str().unwrap_or("").to_string(),
        mode: r["mode"].as_str().unwrap_or("").to_string(),
        title: r["title"].as_str().unwrap_or("").to_string(),
        messages_json: display,
        has_pending_plan: r["has_pending_plan"].as_bool().unwrap_or(false),
        created_at: r["created_at"].as_i64().unwrap_or(0),
        updated_at: r["updated_at"].as_i64().unwrap_or(0),
    })
}

#[tauri::command]
pub async fn delete_conversation(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    daemon_delete::<serde_json::Value>(
        &state.http_client,
        &format!("/conversations/{}", urlencoding::encode(&id)),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Database(e))
}

#[tauri::command]
pub async fn update_conversation_title(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    daemon_patch::<_, serde_json::Value>(
        &state.http_client,
        &format!("/conversations/{}/title", urlencoding::encode(&id)),
        &serde_json::json!({"title": title}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Database(e))
}

#[tauri::command]
pub async fn get_or_create_live_chat_conversation(
    state: State<'_, AppState>,
    username: String,
) -> Result<String, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let vault_id = state.get_vault_uuid().await;

    #[derive(Deserialize)]
    struct Resp { id: String }
    let resp = daemon_post::<_, Resp>(
        &state.http_client,
        "/conversations/live-chat",
        &serde_json::json!({"vault_id": vault_id, "account_id": username}),
        tok,
    ).await.map_err(|e| AppError::Database(e))?;
    Ok(resp.id)
}

// ── Display filter ──────────────────────────────────────────────────────────

fn filter_display_messages(messages_json: &str) -> String {
    let Ok(msgs) = serde_json::from_str::<serde_json::Value>(messages_json) else {
        return messages_json.to_string();
    };
    let Some(arr) = msgs.as_array() else { return messages_json.to_string(); };
    let filtered: Vec<&serde_json::Value> = arr.iter().filter(|m| {
        match m["role"].as_str() {
            Some("user") => true,
            Some("assistant") => m["content"].is_string(),
            _ => false,
        }
    }).collect();
    serde_json::to_string(&filtered).unwrap_or_else(|_| messages_json.to_string())
}

