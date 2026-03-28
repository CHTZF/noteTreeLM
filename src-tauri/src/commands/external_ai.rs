#![allow(dead_code)]
use crate::{error::AppError, state::AppState};
use std::time::Duration;
use tauri::{AppHandle, State};

use super::server::ensure_server_running;

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
    let encrypted = crate::api_client::daemon_get_setting(client, tok, &db_key)
        .await
        .unwrap_or_default();
    let key = crate::crypto::decrypt_api_key(&encrypted);
    cache.lock().await.insert(provider.to_string(), key.clone());
    key
}

/// 一次性 LLM 處理（語音後處理）：非串流，等待完整回應後回傳
#[tauri::command]
pub async fn process_with_llm(
    app: AppHandle,
    state: State<'_, AppState>,
    system: String,
    user_content: String,
) -> Result<String, AppError> {
    let base_url = ensure_server_running(state.inner(), &app).await?;
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "messages": [
            {"role": "system",    "content": system},
            {"role": "user",      "content": user_content},
        ],
        "max_tokens": 1024,
        "temperature": 0.3,
        "stream": false,
    });

    let response = client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| AppError::AI(format!("請求 llama-server 失敗：{}", e)))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(AppError::AI(format!(
            "llama-server 回應錯誤 {}：{}",
            status, text
        )));
    }

    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| AppError::AI(format!("解析 llama-server 回應失敗：{}", e)))?;

    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    Ok(text)
}
