use crate::{db::queries, error::AppError, state::AppState};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};
use tokio::io::AsyncReadExt;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// 從 DB 讀取 llama-server 路徑、模型路徑、埠號
async fn resolve_server_config(state: &AppState) -> Result<(PathBuf, String, u16), AppError> {
    let pool = &state.db;

    let server_path = queries::get_setting(pool, "llama_cli_path")
        .await?
        .unwrap_or_default();
    let model_path = queries::get_setting(pool, "llm_model_path")
        .await?
        .unwrap_or_default();
    let port = queries::get_setting(pool, "llama_server_port")
        .await?
        .unwrap_or_else(|| "8080".to_string())
        .parse::<u16>()
        .unwrap_or(8080);

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

    Ok((bin, model_path, port))
}

/// 確保 llama-server 正在運行；若未啟動則自動 spawn
/// 回傳 base URL（例如 "http://127.0.0.1:8080"）
async fn ensure_server_running(
    state: &AppState,
    app: &AppHandle,
) -> Result<String, AppError> {
    let (bin, model_path, port) = resolve_server_config(state).await?;
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

/// 串流聊天：透過 llama-server /v1/chat/completions (stream=true)
/// tokens 以 "llm:token" 事件即時推送前端，完成後發送 "llm:done"
#[tauri::command]
pub async fn stream_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    messages: Vec<ChatMessage>,
    system: Option<String>,
) -> Result<String, AppError> {
    let base_url = ensure_server_running(state.inner(), &app).await?;
    let client = reqwest::Client::new();

    // 組裝 OpenAI-compatible messages 陣列
    let mut api_messages: Vec<serde_json::Value> = Vec::new();
    if let Some(sys) = &system {
        api_messages.push(serde_json::json!({"role": "system", "content": sys}));
    }
    for msg in &messages {
        api_messages.push(serde_json::json!({"role": msg.role, "content": msg.content}));
    }

    let body = serde_json::json!({
        "messages": api_messages,
        "max_tokens": 2048,
        "temperature": 0.7,
        "stream": true,
    });

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

    // 解析 SSE 串流：每個 event 以 \n\n 分隔
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
                        if let Some(content) =
                            json["choices"][0]["delta"]["content"].as_str()
                        {
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

    let _ = app.emit(
        "llm:stderr",
        format!("[chat] 完成，回應 {} 字元", full_text.len()),
    );
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
        queries::get_setting(&state.db, "llama_cli_path").await,
        Ok(Some(ref p)) if !p.is_empty()
    ) && matches!(
        queries::get_setting(&state.db, "llm_model_path").await,
        Ok(Some(ref p)) if !p.is_empty()
    );
    if configured {
        let _ = ensure_server_running(state, app).await;
    }
}

/// 手動停止 llama-server（App 退出時也會自動呼叫）
#[tauri::command]
pub async fn stop_llama_server(state: State<'_, AppState>) -> Result<(), AppError> {
    let mut guard = state.llama_server.lock().await;
    if let Some(mut child) = guard.take() {
        child
            .kill()
            .await
            .map_err(|e| AppError::AI(format!("停止 llama-server 失敗：{}", e)))?;
    }
    Ok(())
}

// ─── Vault Agent ──────────────────────────────────────────────────────────────

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
                "description": "讀取指定筆記的完整 Markdown 內容",
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
                format!("{}\n\n[…內容過長，已截斷至 6000 字元]", &content[..6000])
            } else {
                content
            }
        }
        Err(e) => format!("讀取失敗：{}", e),
    }
}

/// 全文搜索 Vault（使用 SQLite FTS5）
async fn tool_search_vault(query: &str, state: &AppState) -> String {
    if query.trim().is_empty() {
        return "請提供搜索關鍵字".to_string();
    }
    // FTS5：多字詞用雙引號包圍
    let fts_query = if query.contains(' ') {
        format!("\"{}\"", query.replace('"', ""))
    } else {
        query.to_string()
    };
    let rows = sqlx::query(
        "SELECT n.path, n.title, n.content
         FROM search_fts
         JOIN notes n ON search_fts.rowid = n.id
         WHERE search_fts MATCH ?1
         ORDER BY bm25(search_fts)
         LIMIT 10",
    )
    .bind(&fts_query)
    .fetch_all(&state.db)
    .await;

    match rows {
        Ok(rows) if rows.is_empty() => format!("未找到包含「{}」的筆記", query),
        Ok(rows) => {
            let lines: Vec<String> = rows
                .iter()
                .map(|r| {
                    let path: String = r.get("path");
                    let title: String = r.get("title");
                    let content: Option<String> = r.get("content");
                    let snippet: String = content
                        .as_deref()
                        .unwrap_or("")
                        .chars()
                        .take(100)
                        .collect();
                    format!("- **{}** ({})\n  {}", title, path, snippet.trim())
                })
                .collect();
            format!("找到 {} 篇相關筆記：\n{}", lines.len(), lines.join("\n"))
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

/// 分派工具調用到對應的實作函式
async fn execute_vault_tool(
    name: &str,
    args: &serde_json::Value,
    state: &AppState,
    vault_path: &str,
) -> String {
    if vault_path.is_empty() {
        return "Vault 未設定，無法執行 Vault 操作".to_string();
    }
    match name {
        "search_vault" => {
            let query = args["query"].as_str().unwrap_or("");
            tool_search_vault(query, state).await
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
        _ => format!("未知工具：{}", name),
    }
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
        _ => format!("執行工具: {}", name),
    }
}

/// Vault Agent 聊天：內建工具調用迴圈，可搜索/讀取/新增/編輯 Vault 中的筆記與資料夾
/// 每次工具調用會發送 "agent:tool_call" 事件讓前端即時顯示進度
#[tauri::command]
pub async fn agent_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    messages: Vec<ChatMessage>,
    system: Option<String>,
) -> Result<String, AppError> {
    let base_url = ensure_server_running(state.inner(), &app).await?;
    let vault_path = state.get_vault_path().await;
    let client = reqwest::Client::new();

    // Agent 系統 prompt：說明工具能力，附上可選的筆記上下文
    let agent_system = format!(
        "你是一個能操作 Vault 筆記庫的智慧助手。\
Vault 中的筆記為 Markdown 格式（.md 副檔名），以資料夾階層組織，路徑使用 / 分隔。\
你有以下工具可以使用，請在需要時主動調用：\n\
- search_vault：全文搜索 Vault 中的筆記\n\
- list_structure：列出指定資料夾的子資料夾和筆記，path 傳空字串表示根目錄\n\
- read_note：讀取指定筆記的完整內容\n\
- create_note：在 Vault 中建立新筆記\n\
- update_note：更新現有筆記的完整內容\n\
- create_folder：建立新資料夾\n\
搜索或查詢後，請綜合結果給出清晰的繁體中文回答。{}",
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

        // 把 assistant 訊息（含 tool_calls 欄位）加入歷史
        llm_messages.push(message.clone());

        let tool_calls_arr = message["tool_calls"].as_array();
        let has_tool_calls =
            tool_calls_arr.map(|arr| !arr.is_empty()).unwrap_or(false);

        if finish_reason == "tool_calls" || has_tool_calls {
            // 執行每個工具，把結果送回 LLM
            let calls = tool_calls_arr.cloned().unwrap_or_default();
            for call in &calls {
                let tool_id = call["id"].as_str().unwrap_or("").to_string();
                let tool_name = call["function"]["name"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
                let args: serde_json::Value =
                    serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));

                // 通知前端正在調用哪個工具
                let display = tool_call_display(&tool_name, &args);
                let _ = app.emit("agent:tool_call", &display);

                // 執行工具
                let result =
                    execute_vault_tool(&tool_name, &args, state.inner(), &vault_path).await;

                // 把工具結果加入歷史
                llm_messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_id,
                    "content": result,
                }));
            }
            // 繼續下一輪，把工具結果送回 LLM
        } else {
            // 最終回覆（無更多工具調用）
            let text = message["content"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_string();
            let _ = app.emit("llm:done", &text);
            return Ok(text);
        }
    }

    Err(AppError::AI(
        "Agent 工具調用超過最大輪次（8），請簡化您的請求。".to_string(),
    ))
}
