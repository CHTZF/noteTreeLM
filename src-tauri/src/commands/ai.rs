use crate::{db::queries, error::AppError, state::AppState};
use chrono::{Datelike, Local};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};
use tokio::io::AsyncReadExt;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// 從可用 port 開始往上尋找第一個空閒的 localhost port
fn find_free_port(preferred: u16) -> u16 {
    use std::net::TcpListener;
    for port in preferred..=65535 {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    preferred
}

/// 從 DB 讀取 llama-server 路徑、模型路徑（port 由執行時自動分配）
async fn resolve_server_config(state: &AppState) -> Result<(PathBuf, String), AppError> {
    let pool = &state.settings_db;

    let server_path = queries::get_setting(pool, "llama_cli_path")
        .await?
        .unwrap_or_default();
    let model_path = queries::get_setting(pool, "llm_model_path")
        .await?
        .unwrap_or_default();

    if server_path.is_empty() {
        return Err(AppError::AI(
            "尚未設定 llama-server 路徑，請到 Settings > AI 設定。".to_string(),
        ));
    }
    if model_path.is_empty() {
        return Err(AppError::AI(
            "尚未設定本地 LLM 模型路徑，請到 Settings > AI 設定。".to_string(),
        ));
    }

    let bin = PathBuf::from(&server_path);
    if !bin.exists() {
        return Err(AppError::AI(format!(
            "找不到 llama-server：{}",
            bin.display()
        )));
    }

    Ok((bin, model_path))
}

/// 確保 llama-server 正在運行；若未啟動則自動 spawn
/// 回傳 base URL（例如 "http://127.0.0.1:8080"）
async fn ensure_server_running(
    state: &AppState,
    app: &AppHandle,
) -> Result<String, AppError> {
    // 使用者主動停止旗標：若為 true，拒絕自動重啟
    if state.llama_user_stopped.load(Ordering::SeqCst) {
        return Err(AppError::AI("llama-server 已手動停止".to_string()));
    }

    // 啟動鎖：確保同一時刻只有一個呼叫者在跑啟動 / 等待流程
    let _start_lock = state.llama_start_lock.lock().await;

    let (bin, model_path) = resolve_server_config(state).await?;

    // 取得或自動分配 port（只分配一次，後續重用）
    // port_allocator 確保 whisper 與 llama 不會並發 find_free_port 取到同一 port
    let port = {
        let _alloc_lock = state.port_allocator.lock().await;
        let mut guard = state.llama_actual_port.lock().await;
        if let Some(p) = *guard {
            p
        } else {
            let p = find_free_port(8080);
            *guard = Some(p);
            p
        }
    };

    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::new();

    // 若 state 有子進程，先 ping 確認還活著
    {
        let guard = state.llama_server.lock().await;
        if guard.is_some() {
            let alive = client
                .get(format!("{}/health", base_url))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if alive {
                return Ok(base_url);
            }
            let _ = app.emit("llm:stderr", "[server] 伺服器意外退出，重新啟動…");
        }
    }

    // 啟動新的 llama-server 進程
    let _ = app.emit(
        "llm:stderr",
        format!(
            "[server] 啟動 llama-server：{}\n  模型：{}\n  埠：{}",
            bin.display(),
            model_path,
            port
        ),
    );

    let mut child = tokio::process::Command::new(&bin)
        .args([
            "--model",
            &model_path,
            "--port",
            &port.to_string(),
            "--host",
            "127.0.0.1",
            "--ctx-size",
            "4096",
            "--parallel",
            "1",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| AppError::AI(format!("llama-server 啟動失敗：{}", e)))?;

    // 把 stderr 交給背景 task，轉發為 llm:stderr 事件
    if let Some(mut stderr) = child.stderr.take() {
        let app_stderr = app.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                        let _ = app_stderr.emit("llm:stderr", &chunk);
                    }
                }
            }
        });
    }

    // 存入 state
    {
        let mut guard = state.llama_server.lock().await;
        *guard = Some(child);
    }

    // 輪詢 /health，最多等 60 秒
    let _ = app.emit("llm:stderr", "[server] 等待模型載入…");
    for i in 0..60u32 {
        tokio::time::sleep(Duration::from_secs(1)).await;

        // 先檢查進程是否已退出或被手動停止
        {
            let mut guard = state.llama_server.lock().await;
            match guard.as_mut() {
                None => {
                    // state 被 stop_llama_server 清空 → 使用者手動停止，立即放棄
                    return Err(AppError::AI("llama-server 已手動停止".to_string()));
                }
                Some(child) => {
                    if let Ok(Some(status)) = child.try_wait() {
                        *guard = None;
                        return Err(AppError::AI(format!(
                            "llama-server 意外退出（code: {:?}），請確認模型路徑與二進位設定。",
                            status.code()
                        )));
                    }
                }
            }
        }

        let ready = client
            .get(format!("{}/health", base_url))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);

        if ready {
            let _ = app.emit(
                "llm:stderr",
                format!("[server] 就緒（等待 {} 秒）", i + 1),
            );
            return Ok(base_url);
        }

        if i > 0 && i % 10 == 9 {
            let _ = app.emit(
                "llm:stderr",
                format!("[server] 載入中…（已等待 {} 秒）", i + 1),
            );
        }
    }

    Err(AppError::AI(
        "llama-server 啟動超時（60 秒），請確認 llama-server 路徑與模型設定。".to_string(),
    ))
}

/// 封裝 OpenAI-compatible SSE 串流請求，返回 StreamResult
/// 同時處理文字 token（emit llm:token）和 tool call fragments 的累積
async fn send_streaming_request(
    client: &reqwest::Client,
    base_url: &str,
    body: serde_json::Value,
    app: &AppHandle,
) -> Result<StreamResult, AppError> {
    let response = client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(Duration::from_secs(300))
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

    let mut stream = response.bytes_stream();
    let mut sse_buf = String::new();
    let mut full_text = String::new();
    let mut finish_reason = String::from("stop");
    let mut tool_call_chunks: Vec<ToolCallAccumulator> = Vec::new();

    while let Some(item) = stream.next().await {
        let bytes = item.map_err(|e| AppError::AI(format!("串流讀取失敗：{}", e)))?;
        sse_buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(event_end) = sse_buf.find("\n\n") {
            let event = sse_buf[..event_end].to_string();
            sse_buf = sse_buf[event_end + 2..].to_string();

            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" {
                        continue;
                    }
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        let choice = &json["choices"][0];

                        // 記錄 finish_reason
                        if let Some(fr) = choice["finish_reason"].as_str() {
                            if !fr.is_empty() {
                                finish_reason = fr.to_string();
                            }
                        }

                        let delta = &choice["delta"];

                        // 一般文字 token
                        if let Some(content) = delta["content"].as_str() {
                            if !content.is_empty() {
                                let _ = app.emit("llm:token", content);
                                full_text.push_str(content);
                            }
                        }

                        // Tool call fragments 累積
                        if let Some(tc_arr) = delta["tool_calls"].as_array() {
                            for tc_chunk in tc_arr {
                                let idx =
                                    tc_chunk["index"].as_u64().unwrap_or(0) as usize;
                                while tool_call_chunks.len() <= idx {
                                    tool_call_chunks.push(ToolCallAccumulator {
                                        id: String::new(),
                                        name: String::new(),
                                        arguments: String::new(),
                                    });
                                }
                                let acc = &mut tool_call_chunks[idx];
                                if let Some(id) = tc_chunk["id"].as_str() {
                                    if !id.is_empty() {
                                        acc.id = id.to_string();
                                    }
                                }
                                if let Some(name) = tc_chunk["function"]["name"].as_str() {
                                    if !name.is_empty() {
                                        acc.name = name.to_string();
                                    }
                                }
                                if let Some(args_frag) =
                                    tc_chunk["function"]["arguments"].as_str()
                                {
                                    acc.arguments.push_str(args_frag);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(StreamResult {
        full_text,
        finish_reason,
        tool_call_chunks,
    })
}

/// 回傳只包含 call_external_ai 的工具定義陣列（用於 stream_chat）
fn external_ai_tool_definition() -> serde_json::Value {
    serde_json::json!([{
        "type": "function",
        "function": {
            "name": "call_external_ai",
            "description": "呼叫外部 AI 服務獲取即時資訊或當前事件。\
僅在需要本地模型不具備的最新外部資料時使用（如今日新聞、即時排行、最新活動）。\
不用於查詢 Vault 筆記或歷史對話記憶。",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "發送給外部 AI 的完整問題或指令"
                    }
                },
                "required": ["query"]
            }
        }
    }])
}

/// 串流聊天：透過 llama-server /v1/chat/completions (stream=true)
/// tokens 以 "llm:token" 事件即時推送前端，完成後發送 "llm:done"
#[tauri::command]
pub async fn stream_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    messages: Vec<ChatMessage>,
    system: Option<String>,
    use_tools: Option<bool>,
) -> Result<String, AppError> {
    let base_url = ensure_server_running(state.inner(), &app).await?;
    let client = reqwest::Client::new();

    // 讀取外部 AI 設定
    let settings_db = &state.settings_db;
    let ext_provider = queries::get_setting(settings_db, "ai_provider")
        .await?
        .unwrap_or_default();
    let ext_config = if !ext_provider.is_empty() {
        let base = queries::get_setting(settings_db, "ai_base_url")
            .await?
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let model = queries::get_setting(settings_db, "ai_model")
            .await?
            .unwrap_or_else(|| "gpt-4o".to_string());
        let api_key = read_api_key_sync(&ext_provider);
        ExtAiConfig { provider: ext_provider, base_url: base, model, api_key }
    } else {
        ExtAiConfig {
            provider: String::new(),
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
        }
    };

    // 取得 vault 資訊（Vault 未設定時 vault_db_opt 為 None，vault_path 為空字串）
    let vault_path = state.get_vault_path().await;
    let vault_db_opt = state.get_vault_db().await.ok();

    // 組裝初始 messages 陣列
    let mut messages_json: Vec<serde_json::Value> = Vec::new();
    if let Some(sys) = &system {
        messages_json.push(serde_json::json!({"role": "system", "content": sys}));
    }
    for msg in &messages {
        messages_json.push(serde_json::json!({"role": msg.role, "content": msg.content}));
    }

    let mut final_text = String::new();

    // use_tools=false → Live Chat / 純對話模式：單輪無工具直接串流
    if use_tools == Some(false) {
        let body = serde_json::json!({
            "messages": messages_json,
            "max_tokens": 2048,
            "temperature": 0.7,
            "stream": true,
        });
        let result = send_streaming_request(&client, &base_url, body, &app).await?;
        final_text = result.full_text;
        let _ = app.emit("llm:stderr", format!("[chat] 完成，回應 {} 字元", final_text.len()));
        let _ = app.emit("llm:done", &final_text);
        return Ok(final_text);
    }

    let tools = vault_tools(); // 所有 8 個工具

    // 多輪迴圈（最多 5 輪工具調用）
    for _round in 0..5 {
        let body = serde_json::json!({
            "messages": messages_json,
            "tools": tools,
            "tool_choice": "auto",
            "max_tokens": 2048,
            "temperature": 0.7,
            "stream": true,
        });

        let result = send_streaming_request(&client, &base_url, body, &app).await?;

        // 無 tool call → 完成
        let Some((tool_id, tool_name, tool_args)) = detect_tool_call(&result) else {
            final_text = result.full_text;
            break;
        };

        // 顯示工具呼叫進度給前端
        let display = tool_call_display(&tool_name, &tool_args);
        let _ = app.emit("agent:tool_call", &display);

        // 寫入工具：等待前端確認（存入 oneshot sender，等候 confirm_write_tool 命令）
        let approved = if is_write_tool(&tool_name) {
            let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
            *state.write_confirm_tx.lock().await = Some(tx);
            let _ = app.emit("agent:write_request", &display);
            tokio::time::timeout(Duration::from_secs(60), rx)
                .await
                .unwrap_or(Ok(false))
                .unwrap_or(false)
        } else {
            true
        };

        let tool_result = if approved {
            execute_vault_tool(
                &tool_name,
                &tool_args,
                vault_db_opt.as_ref(),
                &vault_path,
                &app,
                &ext_config,
            )
            .await
        } else {
            "用戶拒絕了此寫入操作。".to_string()
        };

        // 若工具是筆記相關的讀取操作，emit note 絕對路徑供前端導航
        if approved && !vault_path.is_empty() {
            let note_abs_paths: Vec<String> = match tool_name.as_str() {
                "read_note" => {
                    if let Some(p) = tool_args["path"].as_str() {
                        vec![std::path::PathBuf::from(&vault_path)
                            .join(p)
                            .to_string_lossy()
                            .to_string()]
                    } else {
                        vec![]
                    }
                }
                "search_vault" => {
                    // 從結果中解析 "- **title** (rel_path.md)" 格式取出相對路徑
                    tool_result
                        .lines()
                        .filter_map(|line| {
                            let line = line.trim();
                            if line.starts_with("- **") {
                                if let Some(lp) = line.rfind('(') {
                                    if let Some(rp) = line[lp..].find(')') {
                                        let rel = &line[lp + 1..lp + rp];
                                        if rel.ends_with(".md") {
                                            let abs = std::path::PathBuf::from(&vault_path).join(rel);
                                            return Some(abs.to_string_lossy().to_string());
                                        }
                                    }
                                }
                            }
                            None
                        })
                        .collect()
                }
                _ => vec![],
            };
            if !note_abs_paths.is_empty() {
                let _ = app.emit("agent:note_refs", &note_abs_paths);
            }
        }

        // open_note 是純 UI 操作，不需要再讓 LLM 生成下一輪回覆。
        // 直接用工具回傳值當 final_text break 迴圈，避免空字串或無限 tool-call 迴圈。
        if tool_name == "open_note" {
            final_text = tool_result;
            break;
        }

        // 注入工具結果到 messages（native / text-fallback 兩種格式）
        if !tool_id.is_empty() {
            // Native OpenAI tool_calls 格式
            messages_json.push(serde_json::json!({
                "role": "assistant",
                "content": serde_json::Value::Null,
                "tool_calls": [{
                    "id": tool_id,
                    "type": "function",
                    "function": {
                        "name": tool_name,
                        "arguments": serde_json::to_string(&tool_args).unwrap_or_default()
                    }
                }]
            }));
            messages_json.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": tool_id,
                "content": tool_result
            }));
        } else {
            // 文字格式 fallback：保留前言，以 user 訊息注入結果
            let preamble = result
                .full_text
                .find("<tool_call>")
                .map(|p| result.full_text[..p].trim().to_string())
                .unwrap_or_default();
            if !preamble.is_empty() {
                messages_json
                    .push(serde_json::json!({"role": "assistant", "content": preamble}));
            }
            messages_json.push(serde_json::json!({
                "role": "user",
                "content": format!(
                    "工具執行結果：\n\n[{}]\n{}",
                    tool_name, tool_result
                )
            }));
        }
    }

    let _ = app.emit(
        "llm:stderr",
        format!("[chat] 完成，回應 {} 字元", final_text.len()),
    );
    let _ = app.emit("llm:done", &final_text);
    Ok(final_text)
}

/// 串流聊天（外部 AI 提供商）：OpenAI / Anthropic / Ollama
/// tokens 以 "llm:token" 事件推送，完成後發送 "llm:done"
#[tauri::command]
pub async fn stream_chat_external(
    app: AppHandle,
    messages: Vec<ChatMessage>,
    system: Option<String>,
    provider: String,
    base_url: String,
    model: String,
    api_key: String,
) -> Result<String, AppError> {
    match provider.as_str() {
        "anthropic" => stream_external_anthropic(messages, system, model, api_key, app).await,
        _ => stream_external_openai_compat(messages, system, model, base_url, api_key, app).await,
    }
}

/// OpenAI-compatible SSE 串流（openai、ollama 等）
async fn stream_external_openai_compat(
    messages: Vec<ChatMessage>,
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
                    if data.trim() == "[DONE]" {
                        continue;
                    }
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

    let _ = app.emit("llm:stderr", format!("[外部 AI] 完成，回應 {} 字元", full_text.len()));
    let _ = app.emit("llm:done", &full_text);
    Ok(full_text)
}

/// Anthropic Messages API SSE 串流
async fn stream_external_anthropic(
    messages: Vec<ChatMessage>,
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

    // Anthropic SSE: data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"..."}}
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
/// system 放角色指令，user_content 放待處理文字，分開傳可讓模型正確執行任務而非對話
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

/// App 啟動時呼叫：若已設定路徑則背景預熱 llama-server
pub async fn warmup_llama_server(state: &AppState, app: &AppHandle) {
    let configured = matches!(
        queries::get_setting(&state.settings_db, "llama_cli_path").await,
        Ok(Some(ref p)) if !p.is_empty()
    ) && matches!(
        queries::get_setting(&state.settings_db, "llm_model_path").await,
        Ok(Some(ref p)) if !p.is_empty()
    );
    if !configured {
        return; // 未設定是正常情況，靜默跳過
    }
    if let Err(e) = ensure_server_running(state, app).await {
        // 設定錯誤（執行檔不存在、路徑錯誤等）→ 透過事件通知前端顯示 toast
        let _ = app.emit("llm:stderr", format!("[server:error] {}", e));
    }
}

/// 手動停止 llama-server（App 退出時也會自動呼叫）
#[tauri::command]
pub async fn stop_llama_server(state: State<'_, AppState>) -> Result<(), AppError> {
    // 先設旗標，阻止後續請求（stream_chat 等）在 kill+wait 期間或之後重啟
    state.llama_user_stopped.store(true, Ordering::SeqCst);

    let mut guard = state.llama_server.lock().await;
    if let Some(mut child) = guard.take() {
        child.kill().await.ok();
        // 等待進程真正退出，確保下一次 health ping 不會再回應
        child.wait().await.ok();
    }
    // 清除 port，讓 get_llama_server_status 直接回傳 stopped 而不再 ping
    *state.llama_actual_port.lock().await = None;
    Ok(())
}

/// 查詢 llama-server 狀態："running" | "loading" | "stopped"
#[tauri::command]
pub async fn get_llama_server_status(state: State<'_, AppState>) -> Result<String, AppError> {
    // llama_actual_port 為 None 代表本次 session 從未成功啟動過 server，
    // 不應去 ping 預設 8080（避免誤判孤立進程為 running）
    let port = match *state.llama_actual_port.lock().await {
        Some(p) => p,
        None => return Ok("stopped".to_string()),
    };
    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::new();
    let healthy = client
        .get(format!("{}/health", base_url))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if healthy {
        return Ok("running".to_string());
    }
    let mut guard = state.llama_server.lock().await;
    match guard.as_mut() {
        None => Ok("stopped".to_string()),
        Some(child) => match child.try_wait() {
            Ok(None) => Ok("loading".to_string()),
            _ => {
                *guard = None;
                Ok("stopped".to_string())
            }
        },
    }
}

/// 手動啟動 llama-server
#[tauri::command]
pub async fn start_llama_server(
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), AppError> {
    // 清除停止旗標，允許重新啟動
    state.llama_user_stopped.store(false, Ordering::SeqCst);
    ensure_server_running(state.inner(), &app).await?;
    Ok(())
}

/// 重啟 llama-server（先強制關閉再重新啟動）
#[tauri::command]
pub async fn restart_llama_server(
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), AppError> {
    // 清除停止旗標，允許重新啟動
    state.llama_user_stopped.store(false, Ordering::SeqCst);
    {
        let mut guard = state.llama_server.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill().await.ok();
            // wait() 確保進程真正退出後 OS 才釋放 port，
            // 避免重啟時 orphan check 誤判舊進程仍存活
            child.wait().await.ok();
        }
    }
    ensure_server_running(state.inner(), &app).await?;
    Ok(())
}

// ─── Vault Agent ──────────────────────────────────────────────────────────────

/// 外部 AI 提供商設定（供 agent 工具使用）
struct ExtAiConfig {
    provider: String,
    base_url: String,
    model: String,
    api_key: String,
}

/// 串流過程中累積的單一 tool call 資料
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String, // 累積的 JSON fragment 字串
}

/// send_streaming_request 的回傳結果
struct StreamResult {
    full_text: String,
    finish_reason: String,
    tool_call_chunks: Vec<ToolCallAccumulator>,
}

/// 從系統 keyring 讀取 API 金鑰（同步）
fn read_api_key_sync(provider: &str) -> String {
    if provider.is_empty() {
        return String::new();
    }
    keyring::Entry::new("com.notetreelm.app", provider)
        .ok()
        .and_then(|e| e.get_password().ok())
        .unwrap_or_default()
}

/// Tool：呼叫外部 AI（非串流），返回回應文字作為工具結果
async fn call_external_ai_tool(query: &str, config: &ExtAiConfig, app: &AppHandle) -> String {
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
            // OpenAI-compatible（openai、ollama 等）
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

/// 工具定義（OpenAI function calling 格式）
fn vault_tools() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "search_vault",
                "description": "全文搜索 Vault 中的筆記，返回相關筆記列表及摘要",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "搜索關鍵字" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_structure",
                "description": "列出指定資料夾路徑下的子資料夾和筆記（.md）。path 傳空字串表示 Vault 根目錄。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相對於 Vault 根目錄的資料夾路徑（空字串 = 根目錄）" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_note",
                "description": "讀取指定筆記的完整 Markdown 內容，用於需要分析或摘要筆記內容時。\
注意：若使用者只是要「打開」或「查看」筆記，請改用 open_note 工具；read_note 僅用於需要理解筆記內容才能回答問題的情況。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "筆記相對路徑（含 .md 副檔名，例如 工作/專案A.md）" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "create_note",
                "description": "在 Vault 中建立新筆記，會自動建立所需的父資料夾",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "新筆記的相對路徑（含 .md 副檔名）" },
                        "content": { "type": "string", "description": "筆記的 Markdown 內容" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "update_note",
                "description": "覆寫更新現有筆記的完整內容",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "筆記相對路徑" },
                        "content": { "type": "string", "description": "新的完整 Markdown 內容" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "create_folder",
                "description": "在 Vault 中建立新資料夾（含所有中間層資料夾）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "新資料夾的相對路徑" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "query_memory",
                "description": "查詢過去整理的對話記憶筆記。當使用者提到之前討論過的話題、或需要參考過去對話內容時使用。回傳符合的記憶筆記清單（含時間與摘要片段）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "keywords": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "搜尋關鍵字列表，例如 [\"Rust\", \"async\", \"Tauri\"]"
                        },
                        "since": {
                            "type": "string",
                            "description": "可選，只查詢此日期之後的記憶，格式 YYYY-MM-DD"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "回傳最多幾筆，預設 3"
                        }
                    },
                    "required": ["keywords"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "call_external_ai",
                "description": "呼叫外部 AI 服務（如 OpenAI / Anthropic）獲取即時資訊或當前事件。\
僅在問題需要本地模型不具備的最新外部資料時使用（例如今日新聞、即時排行、最新活動等）。\
不用於查詢 Vault 筆記或歷史對話記憶（那些請用 search_vault / query_memory）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "發送給外部 AI 的完整問題或指令"
                        }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "open_note",
                "description": "在筆記編輯器中打開（切換至）指定筆記，讓使用者在編輯器中直接看到內容。\
使用者說「打開」「開啟」「跳轉到」「要查看」「幫我看」「看一下」某筆記時，優先使用此工具，不要用 read_note。\
若不確定路徑，先用 search_vault 找到路徑再呼叫。呼叫後只需回覆「已打開 xxx 筆記」，不要輸出任何筆記內容。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "筆記的相對路徑，例如 'folder/note.md'"
                        }
                    },
                    "required": ["path"]
                }
            }
        }
    ])
}

/// 驗證相對路徑安全性（防止路徑穿越），返回絕對路徑
fn resolve_vault_path(rel_path: &str, vault_path: &str) -> Result<PathBuf, String> {
    if rel_path.contains("..") {
        return Err("不允許路徑穿越（..）".to_string());
    }
    let abs = PathBuf::from(vault_path).join(rel_path);
    if abs.starts_with(vault_path) {
        Ok(abs)
    } else {
        Err("路徑超出 Vault 範圍".to_string())
    }
}

/// 列出指定資料夾的子資料夾和 .md 筆記（單層）
fn tool_list_structure(rel_path: &str, vault_path: &str) -> String {
    let abs_path = if rel_path.is_empty() {
        PathBuf::from(vault_path)
    } else {
        match resolve_vault_path(rel_path, vault_path) {
            Ok(p) => p,
            Err(e) => return e,
        }
    };
    if !abs_path.is_dir() {
        return format!("路徑不存在或不是資料夾：{}", rel_path);
    }
    let mut folders: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&abs_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if entry.path().is_dir() {
                folders.push(format!("[📁] {}", name));
            } else if name.ends_with(".md") {
                notes.push(format!("[📄] {}", name));
            }
        }
    }
    folders.sort();
    notes.sort();
    let label = if rel_path.is_empty() { "根目錄".to_string() } else { rel_path.to_string() };
    let mut lines = vec![format!("📂 {} 的內容：", label)];
    lines.extend(folders);
    lines.extend(notes);
    if lines.len() == 1 {
        lines.push("（空）".to_string());
    }
    lines.join("\n")
}

/// 讀取筆記內容（最多 6000 字元）
fn tool_read_note(rel_path: &str, vault_path: &str) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::read_to_string(&abs_path) {
        Ok(content) => {
            if content.len() > 6000 {
                // Snap to a valid char boundary — CJK chars are 3 bytes each,
                // so the raw byte index 6000 can land mid-character.
                let mut end = 6000usize;
                while end > 0 && !content.is_char_boundary(end) { end -= 1; }
                format!("{}\n\n[…內容過長，已截斷至約 6000 字元]", &content[..end])
            } else {
                content
            }
        }
        Err(e) => format!("讀取失敗：{}", e),
    }
}

// ── FTS 查詢清洗 ──────────────────────────────────────────────────────────────

/// 去除口語指令詞，只保留核心主題詞
/// 例：「幫我找筆記內 飲料」→「飲料」
fn clean_fts_query(query: &str) -> String {
    // 前綴指令詞（依長度降序，避免短詞先匹配）
    const PREFIXES: &[&str] = &[
        "請幫我搜尋", "請幫我搜索", "請幫我查找", "請幫我找",
        "幫我搜尋", "幫我搜索", "幫我查找", "幫我找",
        "幫我", "請找", "請搜尋", "請搜索", "請查",
        "找一下", "查一下", "搜尋一下", "搜索一下",
        "搜尋", "搜索", "查找", "找找",
        "在筆記中", "在vault中", "在Vault中",
        "筆記內", "筆記裡", "筆記中",
        "vault內", "vault裡", "Vault內", "Vault裡",
        "裡面有", "裡頭有",
    ];
    // 後綴雜訊詞
    const SUFFIXES: &[&str] = &[
        "的筆記", "的資料", "的記錄", "的內容", "的相關筆記",
        "相關的筆記", "相關的資料",
    ];
    // 常見助詞/連接詞（整詞清除）
    const STOPWORDS: &[&str] = &["的", "之", "與", "和", "或", "及"];

    let mut q = query.trim().to_string();

    // 反覆剝除前綴，直到無法再匹配
    loop {
        let before = q.clone();
        for &p in PREFIXES {
            if q.starts_with(p) {
                q = q[p.len()..].trim().to_string();
                break;
            }
        }
        if q == before {
            break;
        }
    }

    // 反覆剝除後綴
    loop {
        let before = q.clone();
        for &s in SUFFIXES {
            if q.ends_with(s) {
                let end = q.len() - s.len();
                q = q[..end].trim().to_string();
                break;
            }
        }
        if q == before {
            break;
        }
    }

    // 去除純助詞開頭（如「的奶茶」→「奶茶」）
    for &w in STOPWORDS {
        if q.starts_with(w) && q.len() > w.len() {
            q = q[w.len()..].trim().to_string();
        }
    }

    if q.is_empty() {
        query.trim().to_string() // 若清洗後為空，退回原始 query
    } else {
        q
    }
}

// ── 數值比較過濾 ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Comparison {
    LessThan(f64),
    LessThanOrEqual(f64),
    GreaterThan(f64),
    GreaterThanOrEqual(f64),
    Equal(f64),
    About(f64), // ±15%
}

impl Comparison {
    fn matches(&self, value: f64) -> bool {
        match self {
            Self::LessThan(v) => value < *v,
            Self::LessThanOrEqual(v) => value <= *v,
            Self::GreaterThan(v) => value > *v,
            Self::GreaterThanOrEqual(v) => value >= *v,
            Self::Equal(v) => (value - v).abs() < 0.01,
            Self::About(v) => (value - v).abs() <= v * 0.15,
        }
    }
    fn label(&self) -> String {
        match self {
            Self::LessThan(v) => format!("< {}", v),
            Self::LessThanOrEqual(v) => format!("≤ {}", v),
            Self::GreaterThan(v) => format!("> {}", v),
            Self::GreaterThanOrEqual(v) => format!("≥ {}", v),
            Self::Equal(v) => format!("= {}", v),
            Self::About(v) => format!("≈ {}", v),
        }
    }
}

/// 從查詢字串中解析比較詞 + 數字，返回 (比較條件, 去掉比較部分後的搜索詞)
fn parse_comparison(query: &str) -> (Option<Comparison>, String) {
    // 有序列表：長詞優先（避免「不超過」被「超過」先匹配）
    let keywords: &[(&str, &str)] = &[
        ("不超過", "lte"), ("不高於", "lte"), ("不大於", "lte"), ("至多", "lte"), ("最多", "lte"),
        ("不低於", "gte"), ("不小於", "gte"), ("至少", "gte"), ("最少", "gte"),
        ("低於", "lt"),  ("小於", "lt"),  ("少於", "lt"),  ("未達", "lt"),  ("不足", "lt"),
        ("高於", "gt"),  ("大於", "gt"),  ("多於", "gt"),  ("超過", "gt"),
        ("等於", "eq"),  ("剛好", "eq"),  ("恰好", "eq"),  ("正好", "eq"),
        ("大約", "about"), ("約為", "about"), ("大概", "about"),
        ("差不多", "about"), ("接近", "about"), ("約莫", "about"), ("左右", "about"),
        ("約", "about"),
    ];
    let units = ["元", "塊", "分", "度", "克", "公克", "公斤", "公升", "毫升",
                 "ml", "ML", "kg", "KG", "g", "G", "L", "km", "KM", "m", "M"];

    for &(word, kind) in keywords {
        if let Some(pos) = query.find(word) {
            let after = query[pos + word.len()..].trim_start();
            // 提取數字（整數或小數）
            let num_end = after
                .find(|c: char| !c.is_ascii_digit() && c != '.')
                .unwrap_or(after.len());
            if num_end == 0 {
                continue;
            }
            if let Ok(val) = after[..num_end].parse::<f64>() {
                let cmp = match kind {
                    "lt"    => Comparison::LessThan(val),
                    "lte"   => Comparison::LessThanOrEqual(val),
                    "gt"    => Comparison::GreaterThan(val),
                    "gte"   => Comparison::GreaterThanOrEqual(val),
                    "eq"    => Comparison::Equal(val),
                    _       => Comparison::About(val),
                };
                let before = query[..pos].trim();
                let rest = &after[num_end..];
                // 跳過單位
                let unit_skip = units
                    .iter()
                    .find(|&&u| rest.starts_with(u))
                    .map(|u| u.len())
                    .unwrap_or(0);
                // 去除助詞「的」「之」
                let after_unit = rest[unit_skip..]
                    .trim_start_matches('的')
                    .trim_start_matches('之')
                    .trim();
                let remaining = match (before.is_empty(), after_unit.is_empty()) {
                    (true,  true)  => String::new(),
                    (true,  false) => after_unit.to_string(),
                    (false, true)  => before.to_string(),
                    (false, false) => format!("{} {}", before, after_unit),
                };
                return (Some(cmp), remaining);
            }
        }
    }
    (None, query.to_string())
}

/// 從文字內容中找出含有數字且符合比較條件的行
fn filter_lines_by_comparison(content: &str, cmp: &Comparison) -> Vec<String> {
    let mut matched = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // 掃描行內所有數字（含小數點）
        let bytes = trimmed.as_bytes();
        let mut i = 0;
        let mut found = false;
        while i < bytes.len() && !found {
            if bytes[i].is_ascii_digit() {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                if let Ok(s) = std::str::from_utf8(&bytes[start..i]) {
                    if let Ok(val) = s.parse::<f64>() {
                        if cmp.matches(val) {
                            matched.push(trimmed.to_string());
                            found = true;
                        }
                    }
                }
            } else {
                i += 1;
            }
        }
    }
    matched
}

/// 全文搜索 Vault（使用 SQLite FTS5），支援比較條件過濾
async fn tool_search_vault(query: &str, vault_db: &sqlx::SqlitePool, app: &AppHandle) -> String {
    if query.trim().is_empty() {
        return "請提供搜索關鍵字".to_string();
    }

    // 1. 先清洗口語指令詞
    let cleaned = clean_fts_query(query);
    // 2. 再解析比較條件，提取核心搜索詞
    let (cmp, search_query) = parse_comparison(&cleaned);
    let fts_query = {
        let q = if search_query.trim().is_empty() {
            cleaned.clone()
        } else {
            search_query.trim().to_string()
        };
        // 再次清洗（比較詞剝除後可能遺留助詞）
        clean_fts_query(&q)
    };

    // Debug：顯示搜索細節
    let _ = app.emit(
        "llm:stderr",
        format!(
            "[search] 原始 query: {:?}　→　清洗後: {:?}　→　FTS: {:?}　比較條件: {}",
            query,
            cleaned,
            fts_query,
            cmp.as_ref().map(|c| c.label()).unwrap_or_else(|| "無".to_string())
        ),
    );

    let rows = sqlx::query(
        "SELECT path, title
         FROM search_fts
         WHERE search_fts MATCH ?1
         ORDER BY bm25(search_fts)
         LIMIT 15",
    )
    .bind(&fts_query)
    .fetch_all(vault_db)
    .await;

    match rows {
        Ok(rows) if rows.is_empty() => {
            format!("未找到包含「{}」的筆記", fts_query)
        }
        Ok(rows) => {
            let mut result_lines = Vec::new();
            for r in &rows {
                let path: String = r.get("path");
                let title: String = r.get("title");
                let content: Option<String> = sqlx::query_scalar(
                    "SELECT content FROM notes WHERE path = ?",
                )
                .bind(&path)
                .fetch_optional(vault_db)
                .await
                .ok()
                .flatten();

                let snippet = if let Some(ref c) = content {
                    if let Some(ref cmp_ref) = cmp {
                        // 有比較條件：只列出符合數值的行
                        let matched = filter_lines_by_comparison(c, cmp_ref);
                        if matched.is_empty() {
                            continue; // 此筆記無符合條件的行，略過
                        }
                        format!("（符合條件的行）\n{}", matched.join("\n"))
                    } else {
                        // 一般 snippet：找關鍵字前後文
                        let q = fts_query.to_lowercase();
                        let cl = c.to_lowercase();
                        if let Some(pos) = cl.find(&q) {
                            // Snap to char boundaries — CJK chars are 3 bytes each,
                            // so raw arithmetic offsets can land mid-character.
                            let mut start = pos.saturating_sub(60);
                            while start > 0 && !c.is_char_boundary(start) { start -= 1; }
                            let mut end = (pos + q.len() + 100).min(c.len());
                            while end < c.len() && !c.is_char_boundary(end) { end += 1; }
                            format!("...{}...", c[start..end].trim())
                        } else {
                            c.chars().take(120).collect::<String>() + "..."
                        }
                    }
                } else {
                    String::new()
                };

                result_lines.push(format!("- **{}** ({})\n  {}", title, path, snippet));
            }

            if result_lines.is_empty() {
                format!(
                    "在「{}」相關筆記中，未找到數值{}的項目",
                    fts_query,
                    cmp.as_ref().map(|c| c.label()).unwrap_or_default()
                )
            } else {
                let header = if let Some(ref c) = cmp {
                    format!(
                        "搜索「{}」，篩選數值{}，找到 {} 筆：",
                        fts_query,
                        c.label(),
                        result_lines.len()
                    )
                } else {
                    format!("找到 {} 篇相關筆記：", result_lines.len())
                };
                format!("{}\n{}", header, result_lines.join("\n"))
            }
        }
        Err(e) => format!("搜索失敗：{}", e),
    }
}

/// 建立新筆記（自動建立父資料夾）
async fn tool_create_note(rel_path: &str, content: &str, vault_path: &str) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Some(parent) = abs_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    match tokio::fs::write(&abs_path, content).await {
        Ok(_) => format!("✅ 已建立筆記：{}", rel_path),
        Err(e) => format!("建立失敗：{}", e),
    }
}

/// 更新現有筆記（覆寫全文）
async fn tool_update_note(rel_path: &str, content: &str, vault_path: &str) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match tokio::fs::write(&abs_path, content).await {
        Ok(_) => format!("✅ 已更新筆記：{}", rel_path),
        Err(e) => format!("更新失敗：{}", e),
    }
}

/// 建立資料夾
async fn tool_create_folder(rel_path: &str, vault_path: &str) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match tokio::fs::create_dir_all(&abs_path).await {
        Ok(_) => format!("✅ 已建立資料夾：{}", rel_path),
        Err(e) => format!("建立失敗：{}", e),
    }
}

/// 判斷工具是否為寫入操作（需要使用者確認）
fn is_write_tool(name: &str) -> bool {
    matches!(name, "create_note" | "update_note" | "create_folder")
}

/// 從 StreamResult 提取 tool call（native 格式優先，fallback 文字格式）
/// 回傳 (tool_id, tool_name, tool_args)
fn detect_tool_call(
    result: &StreamResult,
) -> Option<(String, String, serde_json::Value)> {
    // Native OpenAI tool_calls 格式
    if result.finish_reason == "tool_calls" && !result.tool_call_chunks.is_empty() {
        let acc = &result.tool_call_chunks[0];
        let args: serde_json::Value =
            serde_json::from_str(&acc.arguments).unwrap_or(serde_json::json!({}));
        return Some((acc.id.clone(), acc.name.clone(), args));
    }
    // 文字格式 fallback <tool_call>...</tool_call>
    if result.full_text.contains("<tool_call>") {
        if let Some(call) = parse_text_tool_calls(&result.full_text).into_iter().next() {
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
            let args: serde_json::Value =
                serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
            return Some((String::new(), name, args));
        }
    }
    None
}

/// 分派工具調用到對應的實作函式
async fn execute_vault_tool(
    name: &str,
    args: &serde_json::Value,
    vault_db: Option<&sqlx::SqlitePool>,
    vault_path: &str,
    app: &AppHandle,
    ext_config: &ExtAiConfig,
) -> String {
    // call_external_ai 不依賴 vault，可在 vault 未設定時使用
    if name == "call_external_ai" {
        let query = args["query"].as_str().unwrap_or("");
        return call_external_ai_tool(query, ext_config, app).await;
    }

    if vault_path.is_empty() {
        return "Vault 未設定，無法執行 Vault 操作".to_string();
    }
    match name {
        "search_vault" => {
            let query = args["query"].as_str().unwrap_or("");
            match vault_db {
                Some(db) => tool_search_vault(query, db, app).await,
                None => "Vault 資料庫未就緒".to_string(),
            }
        }
        "list_structure" => {
            let path = args["path"].as_str().unwrap_or("");
            tool_list_structure(path, vault_path)
        }
        "read_note" => {
            let path = args["path"].as_str().unwrap_or("");
            tool_read_note(path, vault_path)
        }
        "create_note" => {
            let path = args["path"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            tool_create_note(path, content, vault_path).await
        }
        "update_note" => {
            let path = args["path"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            tool_update_note(path, content, vault_path).await
        }
        "create_folder" => {
            let path = args["path"].as_str().unwrap_or("");
            tool_create_folder(path, vault_path).await
        }
        "query_memory" => {
            let keywords: Vec<String> = args["keywords"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let since = args["since"].as_str().map(String::from);
            let limit = args["limit"].as_u64().map(|v| v as usize);
            match vault_db {
                Some(db) => tool_query_memory(keywords, since, limit, db).await,
                None => "Vault 資料庫未就緒".to_string(),
            }
        }
        "open_note" => {
            let path = args["path"].as_str().unwrap_or("");
            if path.is_empty() {
                return "請提供筆記路徑".to_string();
            }
            let abs_path = std::path::PathBuf::from(vault_path).join(path);
            let abs_str = abs_path.to_string_lossy().to_string();
            let _ = app.emit("ui:open_note", &abs_str);
            format!("✅ 已打開筆記：{}", path)
        }
        _ => format!("未知工具：{}", name),
    }
}

/// 解析 LLM 以文字格式輸出的工具調用
/// 支援格式：<tool_call>{"name":"func","arguments":{...}}</tool_call>
fn parse_text_tool_calls(content: &str) -> Vec<serde_json::Value> {
    let mut calls = Vec::new();
    let mut remaining = content;
    while let Some(start) = remaining.find("<tool_call>") {
        let after_open = &remaining[start + "<tool_call>".len()..];
        if let Some(end) = after_open.find("</tool_call>") {
            let json_str = after_open[..end].trim();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                let name = v["name"].as_str().unwrap_or("").to_string();
                let args_str = serde_json::to_string(&v["arguments"])
                    .unwrap_or_else(|_| "{}".to_string());
                calls.push(serde_json::json!({
                    "id": format!("call_{}", name),
                    "type": "function",
                    "function": { "name": name, "arguments": args_str }
                }));
            }
            remaining = &after_open[end + "</tool_call>".len()..];
        } else {
            break;
        }
    }
    calls
}

/// 工具調用的可讀摘要（發送給前端的 agent:tool_call 事件內容）
fn tool_call_display(name: &str, args: &serde_json::Value) -> String {
    match name {
        "search_vault" => format!("🔍 搜索: \"{}\"", args["query"].as_str().unwrap_or("")),
        "list_structure" => {
            let path = args["path"].as_str().unwrap_or("");
            format!("📂 列出: {}", if path.is_empty() { "根目錄" } else { path })
        }
        "read_note" => format!("📄 讀取: {}", args["path"].as_str().unwrap_or("")),
        "create_note" => format!("✏️  建立筆記: {}", args["path"].as_str().unwrap_or("")),
        "update_note" => format!("✏️  更新筆記: {}", args["path"].as_str().unwrap_or("")),
        "create_folder" => format!("📁 建立資料夾: {}", args["path"].as_str().unwrap_or("")),
        "call_external_ai" => {
            let q = args["query"].as_str().unwrap_or("");
            let preview: String = q.chars().take(50).collect();
            format!("🌐 外部 AI：\"{}\"", preview)
        }
        "open_note" => format!("📂 打開筆記: {}", args["path"].as_str().unwrap_or("")),
        _ => format!("執行工具: {}", name),
    }
}

/// 前端確認/拒絕寫入工具（stream_chat 等待此命令後繼續執行）
#[tauri::command]
pub async fn confirm_write_tool(
    state: State<'_, AppState>,
    approved: bool,
) -> Result<(), AppError> {
    let tx = state.write_confirm_tx.lock().await.take();
    if let Some(tx) = tx {
        let _ = tx.send(approved);
    }
    Ok(())
}

/// Vault Agent 聊天：內建工具調用迴圈，可搜索/讀取/新增/編輯 Vault 中的筆記與資料夾
/// 每次工具調用會發送 "agent:tool_call" 事件讓前端即時顯示進度
/// 工具清單包含 call_external_ai，LLM 可自動決定何時呼叫外部資源
#[tauri::command]
pub async fn agent_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    messages: Vec<ChatMessage>,
    system: Option<String>,
) -> Result<String, AppError> {
    let base_url = ensure_server_running(state.inner(), &app).await?;
    let vault_path = state.get_vault_path().await;
    let vault_db = state.get_vault_db().await?;
    let client = reqwest::Client::new();

    // 讀取外部 AI 設定（provider/model/base_url 存在 settings_db，api_key 存在 keyring）
    let settings_db = &state.settings_db;
    let ext_provider = queries::get_setting(settings_db, "ai_provider")
        .await?.unwrap_or_default();
    let ext_base_url = queries::get_setting(settings_db, "ai_base_url")
        .await?.unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let ext_model = queries::get_setting(settings_db, "ai_model")
        .await?.unwrap_or_else(|| "gpt-4o".to_string());
    let ext_api_key = read_api_key_sync(&ext_provider);
    let ext_config = ExtAiConfig {
        provider: ext_provider,
        base_url: ext_base_url,
        model: ext_model,
        api_key: ext_api_key,
    };

    // Agent 系統 prompt：說明工具能力，附上可選的筆記上下文
    let agent_system = format!(
        "你是一個能操作 Vault 筆記庫的智慧助手，使用繁體中文回答。\
Vault 中的筆記為 Markdown 格式（.md 副檔名），以資料夾階層組織，路徑使用 / 分隔。\n\
\n\
【工具說明】\n\
- search_vault(query)：對筆記標題和內容做全文搜索，query 只能是關鍵字，不支援數字比較運算。\n\
- list_structure(path)：列出資料夾內容，path 傳空字串表示根目錄。\n\
- read_note(path)：讀取指定筆記的完整內容。\n\
- create_note(path, content)：建立新筆記。\n\
- update_note(path, content)：更新現有筆記。\n\
- create_folder(path)：建立新資料夾。\n\
\n\
【搜索策略】\n\
1. search_vault 支援比較條件關鍵字（低於/高於/小於/大於/等於/大約/至少/至多/不超過 + 數字 + 單位），可直接帶入完整條件搜索，例如「奶茶 低於65元」、「飲料 高於100元」。\n\
2. 搜索時帶上核心名詞 + 比較條件，系統會自動過濾筆記中符合數值的行。\n\
3. 若第一次搜索無結果，改用更簡短的同義詞再試一次（例如「飲料」→「奶茶」），不要直接向用戶求助。\n\
4. 整合所有工具結果後給出清晰完整的繁體中文回答，不要要求用戶自己去查。{}",
        system
            .as_deref()
            .map(|s| format!("\n\n---\n目前開啟的筆記內容（供參考）：\n{}", s))
            .unwrap_or_default()
    );

    // 組裝初始 messages
    let mut llm_messages: Vec<serde_json::Value> =
        vec![serde_json::json!({"role": "system", "content": agent_system})];
    for msg in &messages {
        llm_messages.push(serde_json::json!({"role": msg.role, "content": msg.content}));
    }

    let tools = vault_tools();

    // Agent 迴圈（最多 8 輪工具調用）
    for _iter in 0..8 {
        let body = serde_json::json!({
            "messages": llm_messages,
            "tools": tools,
            "tool_choice": "auto",
            "max_tokens": 2048,
            "temperature": 0.7,
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
            .map_err(|e| AppError::AI(format!("解析回應失敗：{}", e)))?;

        let choice = &json["choices"][0];
        let finish_reason = choice["finish_reason"].as_str().unwrap_or("stop");
        let message = &choice["message"];

        let content_str = message["content"].as_str().unwrap_or("").to_string();
        let tool_calls_arr = message["tool_calls"].as_array();
        let has_native_tool_calls = tool_calls_arr.map(|arr| !arr.is_empty()).unwrap_or(false);

        if finish_reason == "tool_calls" || has_native_tool_calls {
            // === 標準 OpenAI tool_calls 格式 ===
            llm_messages.push(message.clone());
            let calls = tool_calls_arr.cloned().unwrap_or_default();
            for call in &calls {
                let tool_id = call["id"].as_str().unwrap_or("").to_string();
                let tool_name = call["function"]["name"].as_str().unwrap_or("").to_string();
                let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
                let args: serde_json::Value =
                    serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                let display = tool_call_display(&tool_name, &args);
                let _ = app.emit("agent:tool_call", &display);
                let result =
                    execute_vault_tool(&tool_name, &args, Some(&vault_db), &vault_path, &app, &ext_config).await;
                llm_messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_id,
                    "content": result,
                }));
            }
        } else {
            // === 嘗試解析文字格式工具調用 <tool_call>...</tool_call> ===
            let text_calls = parse_text_tool_calls(&content_str);
            if !text_calls.is_empty() {
                // 取 <tool_call> 之前的思考前言作為 assistant 內容
                let preamble = content_str
                    .find("<tool_call>")
                    .map(|p| content_str[..p].trim().to_string())
                    .unwrap_or_default();
                llm_messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": preamble
                }));
                let mut results_text = Vec::new();
                for call in &text_calls {
                    let tool_name =
                        call["function"]["name"].as_str().unwrap_or("").to_string();
                    let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
                    let args: serde_json::Value =
                        serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                    let display = tool_call_display(&tool_name, &args);
                    let _ = app.emit("agent:tool_call", &display);
                    let result =
                        execute_vault_tool(&tool_name, &args, Some(&vault_db), &vault_path, &app, &ext_config).await;
                    results_text.push(format!("[工具: {}]\n{}", tool_name, result));
                }
                // 文字格式模型不支援 role:tool，改用 user 訊息傳回結果
                llm_messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!(
                        "以下是工具執行結果，請根據結果回答用戶：\n\n{}",
                        results_text.join("\n\n")
                    )
                }));
            } else {
                // 最終回覆（無更多工具調用）
                let text = content_str.trim().to_string();
                let _ = app.emit("llm:done", &text);
                return Ok(text);
            }
        }
    }

    Err(AppError::AI(
        "Agent 工具調用超過最大輪次（8），請簡化您的請求。".to_string(),
    ))
}

// ─── Memory Agent ─────────────────────────────────────────────────────────────

/// 從查詢字串中提取模式前方的數字（阿拉伯或中文，供 temporal_unit 規則使用）
fn extract_number_before(query: &str, suffix: &str) -> Option<i64> {
    let pos = query.find(suffix)?;
    let before: Vec<char> = query[..pos].chars().collect();
    // 阿拉伯數字（從尾端往前取連續數字）
    let digits: String = before.iter().rev().take_while(|c| c.is_ascii_digit())
        .collect::<String>().chars().rev().collect();
    if !digits.is_empty() { return digits.parse().ok(); }
    // 中文數字（單字）
    let last = before.last()?;
    [('一',1i64),('二',2),('三',3),('四',4),('五',5),
     ('六',6),('七',7),('八',8),('九',9),('十',10)]
        .iter().find(|&&(c,_)| c == *last).map(|&(_,v)| v)
}

/// 載入 memory_rules 表中的規則
async fn load_memory_rules(db: &sqlx::SqlitePool) -> Vec<(String, String, String)> {
    sqlx::query_as::<_, (String, String, String)>(
        "SELECT pattern_type, pattern, value FROM memory_rules ORDER BY id"
    )
    .fetch_all(db).await.unwrap_or_default()
}

/// 與 parse_query_since 相同，但先套用資料庫中的自訂規則
fn parse_query_since_with_rules(
    query: &str,
    now: &chrono::DateTime<Local>,
    rules: &[(String, String, String)],
) -> Option<i64> {
    let day_start = |d: chrono::NaiveDate| -> Option<i64> {
        d.and_hms_opt(0, 0, 0)?.and_local_timezone(Local).earliest()
            .map(|dt| dt.timestamp_millis())
    };
    // 先套用自訂規則
    for (ptype, pattern, value) in rules {
        match ptype.as_str() {
            "temporal_exact_days" if query.contains(pattern.as_str()) => {
                let days: i64 = value.parse().unwrap_or(0);
                return day_start(now.date_naive() + chrono::TimeDelta::days(days));
            }
            "temporal_unit" if query.contains(pattern.as_str()) => {
                if let Some(n) = extract_number_before(query, pattern) {
                    return Some(match value.as_str() {
                        "hours"   => (now.clone() - chrono::TimeDelta::hours(n)).timestamp_millis(),
                        "minutes" => (now.clone() - chrono::TimeDelta::minutes(n)).timestamp_millis(),
                        "weeks"   => return day_start(now.date_naive() - chrono::TimeDelta::weeks(n)),
                        _         => return None,
                    });
                }
            }
            _ => {}
        }
    }
    // 回退到內建規則
    parse_query_since(query, now)
}

/// 與 extract_cjk_bigrams 相同，但合併來自 memory_rules 的額外停用詞
fn extract_cjk_bigrams_with_extra_stops(query: &str, extra_stops: &[char]) -> Vec<String> {
    const STOPS: &[char] = &[
        '你','我','他','她','它','的','了','嗎','啊','哦','嗯','是','有','在',
        '說','知','道','記','得','什','麼','怎','樣','那','這','就','都','也',
        '還','不','沒','要','會','可','以','和','與','或','但','如','果','因',
        '為','所','而','且','雖','然','呢','嘛','吧','喔','囉','啦','呀','嘿',
        '哈','去','來','到','對','把','被','讓','幫','請','謝','好','很',
    ];
    let cjk: Vec<char> = query.chars().filter(|c| is_cjk(*c)).collect();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for pair in cjk.windows(2) {
        let in_stops = |c: char| STOPS.contains(&c) || extra_stops.contains(&c);
        if in_stops(pair[0]) && in_stops(pair[1]) { continue; }
        let bigram: String = pair.iter().collect();
        if seen.insert(bigram.clone()) { out.push(bigram); }
    }
    out
}

/// 將規則寫入 memory_rules 表（供 run_memory_agent 工具呼叫與 add_memory_rule command 共用）
async fn add_memory_rule_to_db(db: &sqlx::SqlitePool, pattern_type: &str, pattern: &str, value: &str) -> String {
    match sqlx::query(
        "INSERT OR REPLACE INTO memory_rules(pattern_type, pattern, value) VALUES (?, ?, ?)"
    )
    .bind(pattern_type).bind(pattern).bind(value)
    .execute(db).await {
        Ok(_)  => format!("規則已儲存：[{}] {} → {}", pattern_type, pattern, value),
        Err(e) => format!("規則儲存失敗：{}", e),
    }
}

/// memory_agent 核心邏輯（取 &AppState，供 Tauri command 與 resolve_memory_context fallback 共用）
///
/// 工具：
///   query_memory    — 搜尋記憶筆記
///   add_memory_rule — 寫入新規則，讓 resolve_memory_context 下次直接處理（自我學習）
async fn run_memory_agent(vault_db: &sqlx::SqlitePool, query: &str, port: u16) -> Result<String, AppError> {
    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::new();

    let now = Local::now();
    let today_str      = now.format("%Y-%m-%d").to_string();
    let yesterday_str  = (now - chrono::TimeDelta::days(1)).format("%Y-%m-%d").to_string();
    let day_before_str = (now - chrono::TimeDelta::days(2)).format("%Y-%m-%d").to_string();
    let this_month_str = now.format("%Y-%m-01").to_string();
    let last_month_str = {
        let (y, m) = (now.year(), now.month());
        if m == 1 { format!("{}-12-01", y - 1) } else { format!("{}-{:02}-01", y, m - 1) }
    };

    let system = format!(
        "你是一個記憶查詢助手。今天是 {today}。\
根據使用者的問題，搜尋相關的過去對話記憶，整理成簡潔摘要後直接輸出。\n\
【時間表達式轉換規則】\n\
- 剛剛／剛才／最近 → keywords=[], 不帶 since\n\
- 今天 → since=\"{today}\"  昨天 → since=\"{yesterday}\"  前天 → since=\"{day_before}\"\n\
- N天前（N≥3）→ since 自行計算  本月 → since=\"{this_month}\"  上月 → since=\"{last_month}\"\n\
- 本週 → since 為本週一（今天是週{weekday}）  X月 → since=\"{{year}}-{{X:02}}-01\"\n\
- 遇到 Rust 不認識的時間表達式（如「3小時前」「大前天」「上上週」）→ 先呼叫 add_memory_rule 儲存規則，再呼叫 query_memory\n\
【add_memory_rule 規則】\n\
  temporal_exact_days: 固定天數，value 為負整數（如「大前天」\"-3\"）\n\
  temporal_unit: 數字+後綴，value 為 hours/minutes/weeks（如「小時前」\"hours\"）\n\
  stopword: 應過濾的停用字，value 為空字串\n\
【輸出規則】只輸出記憶摘要，不對話，不提問。找不到記憶只回覆「未找到相關記憶」。",
        today = today_str, yesterday = yesterday_str, day_before = day_before_str,
        this_month = this_month_str, last_month = last_month_str,
        weekday = now.weekday().number_from_monday(),
    );

    let tools = serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "query_memory",
                "description": "搜尋過去對話記憶。keywords 空陣列=取最新記憶；有關鍵字=FTS 搜尋。since 為時間下限 YYYY-MM-DD。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "keywords": { "type": "array", "items": { "type": "string" },
                            "description": "搜尋關鍵字，空陣列=最新記憶" },
                        "since":    { "type": "string",  "description": "時間下限 YYYY-MM-DD" },
                        "limit":    { "type": "integer", "description": "最多筆數，預設 3" }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "add_memory_rule",
                "description": "發現 Rust 不認識的時間表達式時，儲存規則讓系統下次直接處理（不再需要 LLM）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern_type": { "type": "string",
                            "enum": ["temporal_exact_days","temporal_unit","stopword"],
                            "description": "規則類型" },
                        "pattern": { "type": "string",
                            "description": "觸發字串，如「大前天」「小時前」" },
                        "value": { "type": "string",
                            "description": "temporal_exact_days: 負整數如\"-3\"；temporal_unit: hours/minutes/weeks；stopword: 空字串" }
                    },
                    "required": ["pattern_type","pattern","value"]
                }
            }
        }
    ]);

    let mut messages: Vec<serde_json::Value> = vec![
        serde_json::json!({"role": "user", "content": query}),
    ];

    // 最多 4 輪（考慮 add_memory_rule + query_memory 共兩次工具）
    for _ in 0..4 {
        let mut api_messages = vec![serde_json::json!({"role": "system", "content": system})];
        api_messages.extend(messages.iter().cloned());

        let body = serde_json::json!({
            "model": "local",
            "messages": api_messages,
            "tools": tools,
            "tool_choice": "auto",
            "max_tokens": 1024,
            "stream": false,
        });

        let resp = client.post(format!("{}/v1/chat/completions", base_url))
            .json(&body).timeout(Duration::from_secs(60)).send().await
            .map_err(|e| AppError::AI(format!("memory_agent 請求失敗：{}", e)))?;

        let json: serde_json::Value = resp.json().await
            .map_err(|e| AppError::AI(format!("memory_agent 回應解析失敗：{}", e)))?;

        let message = &json["choices"][0]["message"];
        let content_str = message["content"].as_str().unwrap_or("").to_string();
        let finish_reason = json["choices"][0]["finish_reason"].as_str().unwrap_or("");
        let tool_calls_arr = message["tool_calls"].as_array();
        let has_native_calls = tool_calls_arr.map(|a| !a.is_empty()).unwrap_or(false);

        if finish_reason != "tool_calls" && !has_native_calls {
            let text_calls = parse_text_tool_calls(&content_str);
            if text_calls.is_empty() { return Ok(content_str); }

            messages.push(serde_json::json!({"role": "assistant", "content": content_str}));
            let mut results = Vec::new();
            for call in &text_calls {
                let tool_name = call["function"]["name"].as_str().unwrap_or("");
                let args: serde_json::Value = serde_json::from_str(
                    call["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or_default();
                let r = dispatch_memory_tool(tool_name, &args, vault_db).await;
                results.push(r);
            }
            messages.push(serde_json::json!({
                "role": "user",
                "content": format!("以下是工具結果，請整理後回答：\n\n{}", results.join("\n\n"))
            }));
            continue;
        }

        // 標準 tool_calls 格式
        messages.push(message.clone());
        for call in tool_calls_arr.cloned().unwrap_or_default() {
            let tool_id = call["id"].as_str().unwrap_or("").to_string();
            let tool_name = call["function"]["name"].as_str().unwrap_or("");
            let args: serde_json::Value = serde_json::from_str(
                call["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or_default();
            let result = dispatch_memory_tool(tool_name, &args, vault_db).await;
            messages.push(serde_json::json!({
                "role": "tool", "tool_call_id": tool_id, "content": result
            }));
        }
    }

    Ok("未找到相關記憶".to_string())
}

/// memory_agent / resolve_memory_context 共用工具分派
async fn dispatch_memory_tool(tool_name: &str, args: &serde_json::Value, vault_db: &sqlx::SqlitePool) -> String {
    match tool_name {
        "query_memory" => {
            let keywords: Vec<String> = args["keywords"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let since = args["since"].as_str().map(String::from);
            let limit = args["limit"].as_u64().map(|v| v as usize);
            tool_query_memory(keywords, since, limit, vault_db).await
        }
        "add_memory_rule" => {
            let ptype   = args["pattern_type"].as_str().unwrap_or("");
            let pattern = args["pattern"].as_str().unwrap_or("");
            let value   = args["value"].as_str().unwrap_or("");
            add_memory_rule_to_db(vault_db, ptype, pattern, value).await
        }
        other => format!("未知工具：{}", other),
    }
}

#[tauri::command]
pub async fn memory_agent(state: State<'_, AppState>, query: String) -> Result<String, AppError> {
    let port = queries::get_setting(&state.settings_db, "llama_server_port")
        .await.unwrap_or_default().unwrap_or_else(|| "8080".to_string())
        .parse::<u16>().unwrap_or(8080);
    let vault_db = state.get_vault_db().await?;
    run_memory_agent(&vault_db, &query, port).await
}

/// 讓前端或其他命令可以直接向資料庫新增記憶規則
#[tauri::command]
pub async fn add_memory_rule(
    state: State<'_, AppState>,
    pattern_type: String,
    pattern: String,
    value: String,
) -> Result<(), AppError> {
    let db = state.get_vault_db().await?;
    sqlx::query(
        "INSERT OR REPLACE INTO memory_rules(pattern_type, pattern, value) VALUES (?, ?, ?)"
    )
    .bind(&pattern_type).bind(&pattern).bind(&value)
    .execute(&db).await?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct MemoryRuleEntry {
    pub id: i64,
    pub pattern_type: String,
    pub pattern: String,
    pub value: String,
    pub created_at: i64,
}

/// 取得所有記憶查詢規則（供設定頁面顯示）
#[tauri::command]
pub async fn get_memory_rules(state: State<'_, AppState>) -> Result<Vec<MemoryRuleEntry>, AppError> {
    let db = state.get_vault_db().await?;
    let rows = sqlx::query_as::<_, (i64, String, String, String, i64)>(
        "SELECT id, pattern_type, pattern, value, created_at FROM memory_rules ORDER BY id"
    )
    .fetch_all(&db).await?;
    Ok(rows.into_iter().map(|(id, pattern_type, pattern, value, created_at)| {
        MemoryRuleEntry { id, pattern_type, pattern, value, created_at }
    }).collect())
}

/// 刪除指定 id 的記憶規則
#[tauri::command]
pub async fn delete_memory_rule(state: State<'_, AppState>, id: i64) -> Result<(), AppError> {
    let db = state.get_vault_db().await?;
    sqlx::query("DELETE FROM memory_rules WHERE id = ?")
        .bind(id).execute(&db).await?;
    Ok(())
}

// ─── Memory ───────────────────────────────────────────────────────────────────

/// Agent 工具：查詢記憶筆記（回傳格式化純文字，供 LLM 直接使用）
async fn tool_query_memory(
    keywords: Vec<String>,
    since: Option<String>,
    limit: Option<usize>,
    vault_db: &sqlx::SqlitePool,
) -> String {
    let limit = limit.unwrap_or(3).min(10) as i64;

    // 可選：時間篩選（since 格式 YYYY-MM-DD）
    let since_ts: Option<i64> = since.and_then(|s| {
        chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok().map(|d| {
            d.and_hms_opt(0, 0, 0)
                .and_then(|dt| dt.and_local_timezone(Local).earliest())
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(0)
        })
    });

    let rows: Vec<(String, String, i64)> = if keywords.is_empty() {
        // 無關鍵字：按時間降序回傳最新記憶（不走 FTS，適合「你還記得什麼嗎」類型的問題）
        let mut q = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT path, title, created_at
             FROM notes
             WHERE path LIKE 'memories/ai_memory_%'
             ORDER BY created_at DESC
             LIMIT ?"
        )
        .bind(limit)
        .fetch_all(vault_db)
        .await
        .unwrap_or_default();

        if let Some(min_ts) = since_ts {
            q.retain(|(_, _, ts)| *ts >= min_ts);
        }
        q
    } else {
        // 有關鍵字：用 FTS5 MATCH 搜尋（OR 連接）
        // 注意：FTS5 unicode61 對連續漢字以空格分詞，若關鍵字為多字詞需用引號括起
        let fts_terms: Vec<String> = keywords.iter()
            .map(|k| format!("\"{}\"", k.replace('"', "")))
            .collect();
        let fts_query = fts_terms.join(" OR ");

        let rows = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT s.path, s.title, n.created_at
             FROM search_fts s
             JOIN notes n ON n.path = s.path
             WHERE s.path LIKE 'memories/ai_memory_%'
               AND search_fts MATCH ?
             ORDER BY bm25(search_fts)
             LIMIT ?"
        )
        .bind(&fts_query)
        .bind(limit)
        .fetch_all(vault_db)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .filter(|(_, _, ts)| since_ts.map_or(true, |min_ts| *ts >= min_ts))
            .collect()
    };

    if rows.is_empty() {
        if keywords.is_empty() {
            return "目前沒有任何已儲存的記憶筆記".to_string();
        }
        // FTS 找不到時，降級到最新 1 筆，避免空手而回
        let fallback = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT path, title, created_at
             FROM notes
             WHERE path LIKE 'memories/ai_memory_%'
             ORDER BY created_at DESC
             LIMIT 1"
        )
        .fetch_optional(vault_db)
        .await
        .unwrap_or_default();

        if fallback.is_none() {
            return format!("未找到關鍵字「{}」相關的記憶筆記，且目前無任何記憶存檔", keywords.join("、"));
        }
        // 用最新一筆繼續往下格式化
        let rows_fallback = fallback.into_iter().collect::<Vec<_>>();
        return format_memory_rows(&rows_fallback, &format!("（關鍵字「{}」無精確匹配，以下為最新記憶）", keywords.join("、")), vault_db).await;
    }

    format_memory_rows(&rows, "", vault_db).await
}

async fn format_memory_rows(rows: &[(String, String, i64)], prefix: &str, vault_db: &sqlx::SqlitePool) -> String {
    let mut output = if prefix.is_empty() {
        format!("找到 {} 筆記憶筆記：\n\n", rows.len())
    } else {
        format!("{}\n找到 {} 筆記憶筆記：\n\n", prefix, rows.len())
    };

    for (path, title, created_ms) in rows {
        let dt = chrono::DateTime::from_timestamp_millis(*created_ms)
            .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "未知時間".to_string());

        // 取前 600 字元的摘要（跳過 frontmatter 分隔符）
        let snippet: String = sqlx::query_scalar::<_, String>("SELECT content FROM notes WHERE path = ?")
            .bind(path)
            .fetch_optional(vault_db)
            .await
            .unwrap_or_default()
            .unwrap_or_default()
            .chars()
            .skip_while(|c| *c == '-' || *c == '\n')
            .take(600)
            .collect();

        output.push_str(&format!("【{}】{}\n路徑：{}\n內容：\n{}…\n\n", dt, title, path, snippet.trim()));
    }
    output
}

/// 將當前對話原文儲存為記憶筆記（memories/ai_memory_[timestamp].md）
/// 返回建立的筆記相對路徑
#[tauri::command]
pub async fn save_memory_session(
    state: State<'_, AppState>,
    messages: Vec<ChatMessage>,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("未設定 Vault 路徑".to_string()));
    }

    let now = Local::now();
    let timestamp = now.format("%Y%m%d_%H%M%S").to_string();
    let display_time = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let title = format!("AI 對話記憶 — {}", display_time);
    let rel_path = format!("memories/ai_memory_{}.md", timestamp);

    let db = state.get_vault_db().await?;

    // 確保 memories/ 資料夾存在
    let memories_dir = PathBuf::from(&vault_path).join("memories");
    tokio::fs::create_dir_all(&memories_dir).await
        .map_err(|e| AppError::Vault(format!("建立 memories 資料夾失敗：{}", e)))?;

    // 組裝 Markdown 內容（儲存原始對話）
    let mut content = format!(
        "---\ncreated: {}\nmessage_count: {}\n---\n\n# {}\n\n",
        now.to_rfc3339(),
        messages.iter().filter(|m| m.role != "tool").count(),
        title
    );
    for msg in &messages {
        match msg.role.as_str() {
            "user"      => content.push_str(&format!("**使用者**\n\n{}\n\n---\n\n", msg.content)),
            "assistant" => content.push_str(&format!("**助手**\n\n{}\n\n---\n\n", msg.content)),
            _ => {} // 略過 tool 訊息
        }
    }

    // 寫入磁碟
    let abs_path = PathBuf::from(&vault_path).join(&rel_path);
    tokio::fs::write(&abs_path, &content).await
        .map_err(|e| AppError::Vault(format!("寫入記憶筆記失敗：{}", e)))?;

    // 插入 DB（FTS trigger 會自動同步 search_fts）
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let checksum = format!("{:x}", hasher.finalize());
    let word_count = content.split_whitespace().count() as i64;

    sqlx::query(
        "INSERT OR REPLACE INTO notes(path, title, content, word_count, created_at, modified_at, checksum)
         VALUES (?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&rel_path)
    .bind(&title)
    .bind(&content)
    .bind(word_count)
    .bind(now_ms)
    .bind(now_ms)
    .bind(&checksum)
    .execute(&db)
    .await?;

    sqlx::query(
        "INSERT OR REPLACE INTO graph_nodes(id, node_type, label, created_at)
         VALUES (?, 'note', ?, ?)"
    )
    .bind(&rel_path)
    .bind(&title)
    .bind(now_ms / 1000)
    .execute(&db)
    .await?;

    Ok(rel_path)
}

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
    let limit = limit.unwrap_or(10).min(50) as i64;
    let db = state.get_vault_db().await?;
    if keywords.is_empty() {
        // 無關鍵字時回傳最新的記憶筆記
        let rows = sqlx::query_as::<_, (String, String, i64, String)>(
            "SELECT path, title, created_at, content FROM notes
             WHERE path LIKE 'memories/ai_memory_%'
             ORDER BY created_at DESC
             LIMIT ?"
        )
        .bind(limit)
        .fetch_all(&db)
        .await?;

        return Ok(rows.into_iter().map(|(path, title, created_at, content)| {
            let snippet = content.chars().skip_while(|c| *c == '-' || *c == '\n').take(200).collect();
            MemoryResult { path, title, created_at, snippet }
        }).collect());
    }

    let fts_query = keywords.join(" OR ");
    let rows = sqlx::query_as::<_, (String, String, i64, String)>(
        "SELECT s.path, s.title, n.created_at, n.content
         FROM search_fts s
         JOIN notes n ON n.path = s.path
         WHERE s.path LIKE 'memories/ai_memory_%'
           AND search_fts MATCH ?
         ORDER BY bm25(search_fts)
         LIMIT ?"
    )
    .bind(&fts_query)
    .bind(limit)
    .fetch_all(&db)
    .await
    .unwrap_or_default();

    let since_ts: Option<i64> = since.and_then(|s| {
        chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok().map(|d| {
            d.and_hms_opt(0, 0, 0)
                .and_then(|dt| dt.and_local_timezone(Local).earliest())
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(0)
        })
    });

    let results = rows.into_iter()
        .filter(|(_, _, ts, _)| since_ts.map_or(true, |min_ts| *ts >= min_ts))
        .map(|(path, title, created_at, content)| {
            let snippet = content.chars().skip_while(|c| *c == '-' || *c == '\n').take(200).collect();
            MemoryResult { path, title, created_at, snippet }
        })
        .collect();

    Ok(results)
}

// ─── resolve_memory_context（純 Rust，取代 memory_agent）─────────────────────
//
// 延遲 < 100ms（無 LLM 呼叫）：
//   1. 解析查詢中的時間表達式 → since_ts
//   2. 提取 CJK 雙字元 n-gram → LIKE 條件（繞過 FTS CJK 斷詞問題）
//   3. 降級策略：LIKE 找不到 → 取最新一筆
//   4. 回傳格式化純文字，供 stream_chat system 注入

/// 判斷是否為 CJK 漢字
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF |
        0x20000..=0x2A6DF | 0xF900..=0xFAFF | 0x2F800..=0x2FA1F
    )
}

/// 從查詢字串解析時間表達式，回傳 since 毫秒時間戳（None = 不限時間）
fn parse_query_since(query: &str, now: &chrono::DateTime<Local>) -> Option<i64> {
    use chrono::Datelike;

    let day_start = |d: chrono::NaiveDate| -> Option<i64> {
        d.and_hms_opt(0, 0, 0)?
            .and_local_timezone(Local).earliest()
            .map(|dt| dt.timestamp_millis())
    };

    if query.contains("今天") {
        return day_start(now.date_naive());
    }
    if query.contains("昨天") {
        return day_start(now.date_naive() - chrono::TimeDelta::days(1));
    }
    if query.contains("前天") {
        return day_start(now.date_naive() - chrono::TimeDelta::days(2));
    }

    // "3天前" / "三天前"
    let chars: Vec<char> = query.chars().collect();
    for w in chars.windows(3) {
        if w[1] == '天' && w[2] == '前' {
            let days: Option<i64> = if let Some(d) = w[0].to_digit(10) {
                Some(d as i64)
            } else {
                [('一',1i64),('二',2),('三',3),('四',4),('五',5),
                 ('六',6),('七',7),('八',8),('九',9)]
                    .iter().find(|&&(c, _)| c == w[0]).map(|&(_, v)| v)
            };
            if let Some(d) = days {
                return day_start(now.date_naive() - chrono::TimeDelta::days(d));
            }
        }
    }

    if query.contains("本週") || query.contains("這週") || query.contains("本周") || query.contains("這周") {
        use chrono::Datelike;
        let offset = now.weekday().num_days_from_monday() as i64;
        return day_start(now.date_naive() - chrono::TimeDelta::days(offset));
    }
    if query.contains("本月") || query.contains("這個月") || query.contains("這月") {
        return day_start(chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1)?);
    }
    if query.contains("上個月") || query.contains("上月") {
        let (y, m) = if now.month() == 1 { (now.year() - 1, 12u32) } else { (now.year(), now.month() - 1) };
        return day_start(chrono::NaiveDate::from_ymd_opt(y, m, 1)?);
    }

    // 中文月份 "一月".."十二月" / 阿拉伯 "1月".."12月"
    let cn_months = [("一月",1u32),("二月",2),("三月",3),("四月",4),
                     ("五月",5),("六月",6),("七月",7),("八月",8),
                     ("九月",9),("十月",10),("十一月",11),("十二月",12)];
    for &(name, m) in &cn_months {
        if query.contains(name) {
            return day_start(chrono::NaiveDate::from_ymd_opt(now.year(), m, 1)?);
        }
    }
    for m in 1u32..=12 {
        if query.contains(&format!("{}月", m)) {
            return day_start(chrono::NaiveDate::from_ymd_opt(now.year(), m, 1)?);
        }
    }

    None // 剛剛/剛才/最近/不限時間
}


#[tauri::command]
pub async fn resolve_memory_context(
    state: State<'_, AppState>,
    query: String,
) -> Result<String, AppError> {
    let now = Local::now();
    let db = state.get_vault_db().await?;

    // 1. 載入自訂規則（來自 memory_rules 表，由 memory_agent 自動學習新增）
    let rules = load_memory_rules(&db).await;
    let extra_stops: Vec<char> = rules.iter()
        .filter(|(pt, _, _)| pt == "stopword")
        .filter_map(|(_, pattern, _)| pattern.chars().next())
        .collect();

    // 2. 套用規則解析時間表達式 + 提取搜尋詞
    let since_ts = parse_query_since_with_rules(&query, &now, &rules);
    let terms = extract_cjk_bigrams_with_extra_stops(&query, &extra_stops);

    let limit = 3i64;
    let rows: Vec<(String, String, i64)> = if terms.is_empty() {
        sqlx::query_as::<_, (String, String, i64)>(
            "SELECT path, title, created_at FROM notes
             WHERE path LIKE 'memories/ai_memory_%'
             ORDER BY created_at DESC LIMIT ?"
        )
        .bind(limit).fetch_all(&db).await.unwrap_or_default()
        .into_iter().filter(|(_, _, ts)| since_ts.map_or(true, |min| *ts >= min))
        .collect()
    } else {
        // CJK bigrams → LIKE 搜尋（terms 只含 CJK 字元，無 SQL injection 風險）
        let conditions: String = terms.iter()
            .map(|t| format!("content LIKE '%{}%'", t))
            .collect::<Vec<_>>().join(" OR ");
        let sql = format!(
            "SELECT path, title, created_at FROM notes
             WHERE path LIKE 'memories/ai_memory_%' AND ({})
             ORDER BY created_at DESC LIMIT {}",
            conditions, limit
        );
        let mut rows = sqlx::query_as::<_, (String, String, i64)>(&sql)
            .fetch_all(&db).await.unwrap_or_default()
            .into_iter().filter(|(_, _, ts)| since_ts.map_or(true, |min| *ts >= min))
            .collect::<Vec<_>>();

        // LIKE 無結果 → 降級取最新 1 筆（仍受 since 篩選）
        if rows.is_empty() {
            rows = sqlx::query_as::<_, (String, String, i64)>(
                "SELECT path, title, created_at FROM notes
                 WHERE path LIKE 'memories/ai_memory_%'
                 ORDER BY created_at DESC LIMIT 1"
            )
            .fetch_all(&db).await.unwrap_or_default()
            .into_iter().filter(|(_, _, ts)| since_ts.map_or(true, |min| *ts >= min))
            .collect();
        }
        rows
    };

    // 3. Rust 找到結果 → 直接回傳（快速路徑）
    if !rows.is_empty() {
        return Ok(format_memory_rows(&rows, "", &db).await);
    }

    // 4. Rust 完全找不到（since 過濾後也空） → fallback 到 memory_agent（LLM）
    //    memory_agent 可辨識新時間表達式並呼叫 add_memory_rule 自我學習
    let port = queries::get_setting(&state.settings_db, "llama_server_port")
        .await.unwrap_or_default().unwrap_or_else(|| "8080".to_string())
        .parse::<u16>().unwrap_or(8080);
    run_memory_agent(&db, &query, port).await
}

/// 直接測試單一 Agent 工具，供 debug 面板使用
#[tauri::command]
pub async fn test_vault_tool(
    app: AppHandle,
    state: State<'_, AppState>,
    tool_name: String,
    args: serde_json::Value,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;
    let vault_db = state.get_vault_db().await.ok();

    // Only build ext_config for call_external_ai — other tools ignore it entirely.
    // Avoids unnecessary keychain access (read_api_key_sync) which can block on macOS
    // if the security framework is busy during concurrent operations.
    let ext_config = if tool_name == "call_external_ai" {
        let settings_db = &state.settings_db;
        let ext_provider = queries::get_setting(settings_db, "ai_provider")
            .await.unwrap_or_default().unwrap_or_default();
        let ext_base_url = queries::get_setting(settings_db, "ai_base_url")
            .await.unwrap_or_default()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let ext_model = queries::get_setting(settings_db, "ai_model")
            .await.unwrap_or_default()
            .unwrap_or_else(|| "gpt-4o".to_string());
        let ext_api_key = read_api_key_sync(&ext_provider);
        ExtAiConfig { provider: ext_provider, base_url: ext_base_url, model: ext_model, api_key: ext_api_key }
    } else {
        ExtAiConfig { provider: String::new(), base_url: String::new(), model: String::new(), api_key: String::new() }
    };

    let result = execute_vault_tool(
        &tool_name,
        &args,
        vault_db.as_ref(),
        &vault_path,
        &app,
        &ext_config,
    ).await;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_comparison_basic() {
        let (cmp, q) = parse_comparison("低於65元的飲料");
        assert!(matches!(cmp, Some(Comparison::LessThan(v)) if (v - 65.0).abs() < 0.01));
        assert_eq!(q, "飲料");

        let (cmp, q) = parse_comparison("高於100元的咖啡");
        assert!(matches!(cmp, Some(Comparison::GreaterThan(v)) if (v - 100.0).abs() < 0.01));
        assert_eq!(q, "咖啡");

        let (cmp, q) = parse_comparison("不超過50元的奶茶");
        assert!(matches!(cmp, Some(Comparison::LessThanOrEqual(v)) if (v - 50.0).abs() < 0.01));
        assert_eq!(q, "奶茶");

        let (cmp, q) = parse_comparison("大約30元的紅茶");
        assert!(matches!(cmp, Some(Comparison::About(v)) if (v - 30.0).abs() < 0.01));
        assert_eq!(q, "紅茶");

        let (cmp, q) = parse_comparison("至少80元的果汁");
        assert!(matches!(cmp, Some(Comparison::GreaterThanOrEqual(v)) if (v - 80.0).abs() < 0.01));
        assert_eq!(q, "果汁");
    }

    #[test]
    fn test_parse_comparison_llm_style() {
        // LLM 通常會把比較詞放前面
        let (cmp, q) = parse_comparison("飲料 低於65元");
        assert!(matches!(cmp, Some(Comparison::LessThan(v)) if (v - 65.0).abs() < 0.01));
        assert_eq!(q, "飲料");

        // 無比較詞時原樣返回
        let (cmp, q) = parse_comparison("奶茶");
        assert!(cmp.is_none());
        assert_eq!(q, "奶茶");
    }

    #[test]
    fn test_clean_fts_query() {
        // 完整口語句子 → 核心詞
        assert_eq!(clean_fts_query("幫我找筆記內低於65元的飲料"), "低於65元的飲料");
        assert_eq!(clean_fts_query("搜尋奶茶"), "奶茶");
        assert_eq!(clean_fts_query("請幫我找咖啡的筆記"), "咖啡");
        assert_eq!(clean_fts_query("在筆記中高於100元的果汁"), "高於100元的果汁");
        assert_eq!(clean_fts_query("找一下紅茶"), "紅茶");
        // 無指令詞時原樣
        assert_eq!(clean_fts_query("飲料"), "飲料");
    }

    #[test]
    fn test_full_pipeline() {
        // 模擬完整流程：口語句 → 清洗 → 解析比較 → 最終 FTS 詞
        let simulate = |raw: &str| -> (bool, String) {
            let cleaned = clean_fts_query(raw);
            let (cmp, search_query) = parse_comparison(&cleaned);
            let fts = {
                let q = if search_query.trim().is_empty() { cleaned.clone() } else { search_query.trim().to_string() };
                clean_fts_query(&q)
            };
            (cmp.is_some(), fts)
        };

        let (has_cmp, fts) = simulate("幫我找筆記內低於65元的飲料");
        assert!(has_cmp, "應解析出比較條件");
        assert_eq!(fts, "飲料");

        let (has_cmp, fts) = simulate("搜尋高於100元的咖啡");
        assert!(has_cmp);
        assert_eq!(fts, "咖啡");

        let (has_cmp, fts) = simulate("找一下奶茶");
        assert!(!has_cmp);
        assert_eq!(fts, "奶茶");
    }

    #[test]
    fn test_filter_lines_by_comparison() {
        let content = "\
珍珠奶茶 60元
抹茶拿鐵 75元
草莓奶昔 55元
黑糖鮮奶茶 65元
焦糖瑪奇朵 80元";

        let cmp = Comparison::LessThan(65.0);
        let matched = filter_lines_by_comparison(content, &cmp);
        // 60 < 65 ✓, 75 ✗, 55 < 65 ✓, 65 not < 65 ✗, 80 ✗
        assert_eq!(matched.len(), 2);
        assert!(matched[0].contains("60"));
        assert!(matched[1].contains("55"));

        let cmp = Comparison::LessThanOrEqual(65.0);
        let matched = filter_lines_by_comparison(content, &cmp);
        // 60 ✓, 55 ✓, 65 ≤ 65 ✓
        assert_eq!(matched.len(), 3);

        let cmp = Comparison::About(65.0); // ±15% → 55.25~74.75
        let matched = filter_lines_by_comparison(content, &cmp);
        // 60 ✓, 75 ✓, 55 ✗（54.25邊緣，55.25以上才算）, 65 ✓
        assert!(matched.iter().any(|l| l.contains("60")));
        assert!(matched.iter().any(|l| l.contains("65")));
    }
}
