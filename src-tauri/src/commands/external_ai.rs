#![allow(dead_code)]
use crate::{error::AppError, state::AppState};
use tauri::{AppHandle, State};

/// 設定讀取快取輔助：先查快取，miss 時查 daemon 並寫入快取
pub async fn get_cached_setting(
    cache: &tokio::sync::Mutex<std::collections::HashMap<String, String>>,
    client: &reqwest::Client,
    tok: Option<&str>,
    key: &str,
    default: &str,
) -> String {
    {
        let guard = cache.lock().await;
        if let Some(v) = guard.get(key) {
            return v.clone();
        }
    }
    let val = crate::api_client::daemon_get_setting(client, tok, key)
        .await
        .unwrap_or_else(|| default.to_string());
    cache.lock().await.insert(key.to_string(), val.clone());
    val
}

/// 從記憶體快取讀取 API 金鑰；cache miss 時查 daemon 並回填快取
pub(crate) async fn read_api_key(
    cache: &tokio::sync::Mutex<std::collections::HashMap<String, String>>,
    client: &reqwest::Client,
    tok: Option<&str>,
    provider: &str,
) -> String {
    if provider.is_empty() {
        return String::new();
    }
    {
        let c = cache.lock().await;
        if let Some(k) = c.get(provider) {
            return k.clone();
        }
    }
    let db_key = format!("api_key_{}", provider);
    // Service returns decrypted plaintext for api_key_* keys.
    let key = crate::api_client::daemon_get_setting(client, tok, &db_key)
        .await
        .unwrap_or_default();
    cache.lock().await.insert(provider.to_string(), key.clone());
    key
}

/// 一次性 LLM 處理（語音後處理）：委派給 service /llm/chat
#[tauri::command]
pub async fn process_with_llm(
    _app: AppHandle,
    state: State<'_, AppState>,
    system: String,
    user_content: String,
) -> Result<String, AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };

    let resp = crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/llm/chat",
        &serde_json::json!({"system": system, "user_content": user_content}),
        tok,
    ).await.map_err(|e| AppError::AI(e.to_string()))?;

    Ok(resp["text"].as_str().unwrap_or("").to_string())
}
