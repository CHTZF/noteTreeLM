use crate::{db::queries, error::AppError, state::AppState};
use crate::db::surreal::SurrealDb;
use futures_util::StreamExt;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};

use super::server::ensure_server_running;

/// 外部 AI 提供商設定（供 agent 工具使用）
pub struct ExtAiConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

/// DB 設定讀取快取輔助：先查快取，miss 時讀 DB 並寫入快取
pub async fn get_cached_setting(
    cache: &tokio::sync::Mutex<std::collections::HashMap<String, String>>,
    db: &SurrealDb,
    key: &str,
    default: &str,
) -> String {
    {
        let guard = cache.lock().await;
        if let Some(v) = guard.get(key) {
            return v.clone();
        }
    }
    let val = queries::get_setting(db, key)
        .await.unwrap_or_default()
        .unwrap_or_else(|| default.to_string());
    cache.lock().await.insert(key.to_string(), val.clone());
    val
}

/// 從記憶體快取讀取 API 金鑰；cache miss 時查 keychain 並回填快取
pub(crate) async fn read_api_key(
    cache: &tokio::sync::Mutex<std::collections::HashMap<String, String>>,
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
    let key = keyring::Entry::new("com.notetreelm.app", provider)
        .ok()
        .and_then(|e| e.get_password().ok())
        .unwrap_or_default();
    cache.lock().await.insert(provider.to_string(), key.clone());
    key
}

/// Tool：呼叫外部 AI（非串流），返回回應文字作為工具結果
pub async fn call_external_ai_tool(query: &str, config: &ExtAiConfig, app: &AppHandle) -> String {
    if config.provider.is_empty() {
        return "外部 AI 未設定，請至「設定 > 外部資源」頁面設定提供商與 API 金鑰後再試。".to_string();
    }

    let _ = app.emit(
        "llm:stderr",
        format!("[外部 AI] 查詢：{}", query.chars().take(80).collect::<String>()),
    );

    let client = reqwest::Client::new();
    let messages = vec![serde_json::json!({"role": "user", "content": query})];

    let response_text = match config.provider.as_str() {
        "anthropic" => {
            let body = serde_json::json!({
                "model": config.model,
                "max_tokens": 2048,
                "messages": messages,
            });
            match client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &config.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .timeout(Duration::from_secs(60))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => r
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|j| j["content"][0]["text"].as_str().map(String::from))
                    .unwrap_or_else(|| "外部 AI 回應解析失敗".to_string()),
                Ok(r) => format!(
                    "外部 AI 錯誤 {}：{}",
                    r.status(),
                    r.text().await.unwrap_or_default()
                ),
                Err(e) => format!("外部 AI 請求失敗：{}", e),
            }
        }
        _ => {
            let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
            let body = serde_json::json!({
                "model": config.model,
                "messages": messages,
                "max_tokens": 2048,
            });
            let mut req = client.post(&url).json(&body).timeout(Duration::from_secs(60));
            if !config.api_key.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", config.api_key));
            }
            match req.send().await {
                Ok(r) if r.status().is_success() => r
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|j| {
                        j["choices"][0]["message"]["content"]
                            .as_str()
                            .map(String::from)
                    })
                    .unwrap_or_else(|| "外部 AI 回應解析失敗".to_string()),
                Ok(r) => format!(
                    "外部 AI 錯誤 {}：{}",
                    r.status(),
                    r.text().await.unwrap_or_default()
                ),
                Err(e) => format!("外部 AI 請求失敗：{}", e),
            }
        }
    };

    let _ = app.emit(
        "llm:stderr",
        format!("[外部 AI] 完成，{} 字元", response_text.len()),
    );
    response_text
}

/// 從快取（或 DB）讀取外部 AI 設定並執行查詢（供 ToolRegistry 的 call_external_ai 工具使用）
pub(crate) async fn call_external_ai_via_db(
    query: &str,
    db: &SurrealDb,
    app: &AppHandle,
    api_key_cache: &tokio::sync::Mutex<std::collections::HashMap<String, String>>,
    settings_cache: &tokio::sync::Mutex<std::collections::HashMap<String, String>>,
) -> String {
    let provider = get_cached_setting(settings_cache, db, "ai_provider", "").await;
    let base_url = get_cached_setting(settings_cache, db, "ai_base_url", "https://api.openai.com/v1").await;
    let model = get_cached_setting(settings_cache, db, "ai_model", "gpt-4o").await;
    let api_key = read_api_key(api_key_cache, &provider).await;
    let config = ExtAiConfig { provider, base_url, model, api_key };
    call_external_ai_tool(query, &config, app).await
}

/// 串流聊天（外部 AI 提供商）：OpenAI / Anthropic / Ollama
/// tokens 以 "llm:token" 事件推送，完成後發送 "llm:done"
#[tauri::command]
pub async fn stream_chat_external(
    app: AppHandle,
    state: State<'_, AppState>,
    messages: Vec<super::ai::ChatMessage>,
    system: Option<String>,
    provider: String,
    base_url: String,
    model: String,
    api_key: String,
) -> Result<String, AppError> {
    // KB context injection（同 invoke_agent）
    let enriched_system: Option<String> = {
        let vault_id = state.get_vault_id().await.ok();
        if let Some(ref vid) = vault_id {
            let emb_url: Option<String> = {
                let port = *state.embedding_actual_port.lock().await;
                port.map(|p| format!("http://127.0.0.1:{}", p))
            };
            let kb_query = messages.iter().rev()
                .find(|m| m.role == "user")
                .map(|m| m.content.clone())
                .unwrap_or_default();
            if !kb_query.is_empty() {
                if let Some(kb_ctx) = crate::commands::knowledge_import::search_kb_context(
                    &state.db, vid, &kb_query, emb_url.as_deref()
                ).await {
                    let base = system.as_deref().unwrap_or("");
                    Some(if base.is_empty() { kb_ctx } else { format!("{}\n\n{}", base, kb_ctx) })
                } else { system.clone() }
            } else { system.clone() }
        } else { system.clone() }
    };

    match provider.as_str() {
        "anthropic" => stream_external_anthropic(messages, enriched_system, model, api_key, app).await,
        _ => stream_external_openai_compat(messages, enriched_system, model, base_url, api_key, app).await,
    }
}

/// OpenAI-compatible SSE 串流（openai、ollama 等）
async fn stream_external_openai_compat(
    messages: Vec<super::ai::ChatMessage>,
    system: Option<String>,
    model: String,
    base_url: String,
    api_key: String,
    app: AppHandle,
) -> Result<String, AppError> {
    let client = reqwest::Client::new();
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let mut api_messages: Vec<serde_json::Value> = Vec::new();
    if let Some(sys) = &system {
        api_messages.push(serde_json::json!({"role": "system", "content": sys}));
    }
    for msg in &messages {
        api_messages.push(serde_json::json!({"role": msg.role, "content": msg.content}));
    }

    let body = serde_json::json!({
        "model": model,
        "messages": api_messages,
        "max_tokens": 2048,
        "temperature": 0.7,
        "stream": true,
    });

    let mut req = client.post(&url).json(&body).timeout(Duration::from_secs(120));
    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", api_key));
    }

    let response = req
        .send()
        .await
        .map_err(|e| AppError::AI(format!("外部 AI 請求失敗：{}", e)))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(AppError::AI(format!("外部 AI 回應錯誤 {}：{}", status, text)));
    }

    let mut stream = response.bytes_stream();
    let mut sse_buf = String::new();
    let mut full_text = String::new();

    while let Some(item) = stream.next().await {
        let bytes = item.map_err(|e| AppError::AI(format!("串流讀取失敗：{}", e)))?;
        sse_buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(event_end) = sse_buf.find("\n\n") {
            let event = sse_buf[..event_end].to_string();
            sse_buf = sse_buf[event_end + 2..].to_string();

            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" { continue; }
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
                            if !content.is_empty() {
                                let _ = app.emit("llm:token", content);
                                full_text.push_str(content);
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = app.emit("llm:done", &full_text);
    Ok(full_text)
}

/// Anthropic Messages API SSE 串流
async fn stream_external_anthropic(
    messages: Vec<super::ai::ChatMessage>,
    system: Option<String>,
    model: String,
    api_key: String,
    app: AppHandle,
) -> Result<String, AppError> {
    let client = reqwest::Client::new();
    let url = "https://api.anthropic.com/v1/messages";

    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": 2048,
        "stream": true,
        "messages": messages.iter().map(|m| serde_json::json!({
            "role": m.role,
            "content": m.content,
        })).collect::<Vec<_>>(),
    });
    if let Some(sys) = system {
        body["system"] = serde_json::json!(sys);
    }

    let response = client
        .post(url)
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| AppError::AI(format!("Anthropic 請求失敗：{}", e)))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(AppError::AI(format!("Anthropic 回應錯誤 {}：{}", status, text)));
    }

    let mut stream = response.bytes_stream();
    let mut sse_buf = String::new();
    let mut full_text = String::new();

    while let Some(item) = stream.next().await {
        let bytes = item.map_err(|e| AppError::AI(format!("串流讀取失敗：{}", e)))?;
        sse_buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(event_end) = sse_buf.find("\n\n") {
            let event = sse_buf[..event_end].to_string();
            sse_buf = sse_buf[event_end + 2..].to_string();

            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        if json["type"] == "content_block_delta" {
                            if let Some(text) = json["delta"]["text"].as_str() {
                                if !text.is_empty() {
                                    let _ = app.emit("llm:token", text);
                                    full_text.push_str(text);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = app.emit("llm:stderr", format!("[Anthropic] 完成，回應 {} 字元", full_text.len()));
    let _ = app.emit("llm:done", &full_text);
    Ok(full_text)
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
