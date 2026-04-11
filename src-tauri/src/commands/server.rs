use crate::{error::AppError, state::AppState};
use tauri::{AppHandle, State};

// ─── Internal helpers (used by other Tauri commands) ──────────────────────────

/// 確保 llama-server 啟動（委派給 service）。
/// 僅在呼叫端需要 llama URL 時使用；大多數情況下 service 自行管理。
pub(crate) async fn ensure_server_running(
    state: &AppState,
    _app: &AppHandle,
) -> Result<String, AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };

    let resp = crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/llm/start",
        &serde_json::json!({}),
        tok,
    ).await.map_err(|e| AppError::AI(e.to_string()))?;

    let url = resp["url"].as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::AI("llama-server 未啟動或未設定模型路徑".to_string()))?
        .to_string();
    Ok(url)
}

/// Get single embedding via service API.
pub async fn get_embedding_via_service(state: &AppState, text: &str) -> Vec<f32> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };

    let result = crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/llm/embedding",
        &serde_json::json!({"text": text}),
        tok,
    ).await.unwrap_or_default();

    result["embedding"].as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect())
        .unwrap_or_default()
}

/// App 啟動時呼叫：背景觸發 service 啟動並預熱 llama-server
pub async fn warmup_llama_server(state: &AppState, _app: &AppHandle) {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client, "/llm/start", &serde_json::json!({}), tok,
    ).await;
}

/// App 啟動時呼叫：背景觸發 service 啟動並預熱 embedding-server
pub async fn warmup_embedding_server(state: &AppState, _app: &AppHandle) {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client, "/embedding/start", &serde_json::json!({}), tok,
    ).await;
}

// ─── Tauri commands (thin wrappers) ──────────────────────────────────────────

#[tauri::command]
pub async fn get_llama_server_status(state: State<'_, AppState>) -> Result<String, AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    let val: serde_json::Value = crate::api_client::daemon_get(
        &state.http_client, "/llm/status", tok,
    ).await.unwrap_or_else(|_| serde_json::json!({"status": "stopped"}));
    Ok(val["status"].as_str().unwrap_or("stopped").to_string())
}

#[tauri::command]
pub async fn start_llama_server(state: State<'_, AppState>, _app: AppHandle) -> Result<(), AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client, "/llm/start", &serde_json::json!({}), tok,
    ).await.map_err(|e| AppError::AI(e.to_string()))?;
    Ok(())
}

#[tauri::command]
pub async fn stop_llama_server(state: State<'_, AppState>) -> Result<(), AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client, "/llm/stop", &serde_json::json!({}), tok,
    ).await.map_err(|e| AppError::AI(e.to_string()))?;
    Ok(())
}

#[tauri::command]
pub async fn restart_llama_server(state: State<'_, AppState>, _app: AppHandle) -> Result<(), AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client, "/llm/restart", &serde_json::json!({}), tok,
    ).await.map_err(|e| AppError::AI(e.to_string()))?;
    Ok(())
}

#[tauri::command]
pub async fn get_embedding_server_status(state: State<'_, AppState>) -> Result<String, AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    let val: serde_json::Value = crate::api_client::daemon_get(
        &state.http_client, "/embedding/status", tok,
    ).await.unwrap_or_else(|_| serde_json::json!({"status": "stopped"}));
    Ok(val["status"].as_str().unwrap_or("stopped").to_string())
}

#[tauri::command]
pub async fn check_embedding_endpoint(state: State<'_, AppState>) -> Result<String, AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    let status: serde_json::Value = crate::api_client::daemon_get(
        &state.http_client, "/embedding/status", tok,
    ).await.unwrap_or_else(|_| serde_json::json!({"status": "stopped"}));
    Ok(format!("embedding status: {}", status["status"].as_str().unwrap_or("unknown")))
}

#[tauri::command]
pub async fn start_embedding_server(state: State<'_, AppState>, _app: AppHandle) -> Result<(), AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client, "/embedding/start", &serde_json::json!({}), tok,
    ).await.map_err(|e| AppError::AI(e.to_string()))?;
    Ok(())
}

#[tauri::command]
pub async fn stop_embedding_server(state: State<'_, AppState>) -> Result<(), AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client, "/embedding/stop", &serde_json::json!({}), tok,
    ).await.map_err(|e| AppError::AI(e.to_string()))?;
    Ok(())
}

#[tauri::command]
pub async fn restart_embedding_server(state: State<'_, AppState>, _app: AppHandle) -> Result<(), AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client, "/embedding/restart", &serde_json::json!({}), tok,
    ).await.map_err(|e| AppError::AI(e.to_string()))?;
    Ok(())
}


