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
async fn tool_search_vault(query: &str, state: &AppState, app: &AppHandle) -> String {
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
    .fetch_all(&state.db)
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
                .fetch_optional(&state.db)
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
                            let start = pos.saturating_sub(60);
                            let end = (pos + q.len() + 100).min(c.len());
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

/// 分派工具調用到對應的實作函式
async fn execute_vault_tool(
    name: &str,
    args: &serde_json::Value,
    state: &AppState,
    vault_path: &str,
    app: &AppHandle,
) -> String {
    if vault_path.is_empty() {
        return "Vault 未設定，無法執行 Vault 操作".to_string();
    }
    match name {
        "search_vault" => {
            let query = args["query"].as_str().unwrap_or("");
            tool_search_vault(query, state, app).await
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
                    execute_vault_tool(&tool_name, &args, state.inner(), &vault_path, &app).await;
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
                        execute_vault_tool(&tool_name, &args, state.inner(), &vault_path, &app).await;
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
