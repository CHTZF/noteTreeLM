use crate::{api_client::{daemon_delete, daemon_get, daemon_post}, error::AppError, state::AppState};
use chrono::Local;
use serde::Serialize;
use std::path::PathBuf;
use tauri::State;

use super::ai::ChatMessage;

// ─── Memory Rules ─────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn add_memory_rule(
    state: State<'_, AppState>,
    pattern_type: String,
    pattern: String,
    value: String,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let vault_id = state.get_vault_id().await?;
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/memory/rules", urlencoding::encode(&vault_id)),
        &serde_json::json!({"pattern_type": pattern_type, "pattern": pattern, "value": value}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Database(e))
}

#[derive(Debug, Serialize)]
pub struct MemoryRuleEntry {
    pub id: i64,
    pub pattern_type: String,
    pub pattern: String,
    pub value: String,
    pub created_at: i64,
}

#[tauri::command]
pub async fn get_memory_rules(state: State<'_, AppState>) -> Result<Vec<MemoryRuleEntry>, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let vault_id = state.get_vault_id().await?;
    let rows: Vec<serde_json::Value> = daemon_get(
        &state.http_client,
        &format!("/vaults/{}/memory/rules", urlencoding::encode(&vault_id)),
        tok,
    ).await.unwrap_or_default();
    Ok(rows.into_iter().enumerate().map(|(idx, r)| MemoryRuleEntry {
        id: idx as i64,
        pattern_type: r["pattern_type"].as_str().unwrap_or("").to_string(),
        pattern: r["pattern"].as_str().unwrap_or("").to_string(),
        value: r["value"].as_str().unwrap_or("").to_string(),
        created_at: r["created_at"].as_i64().unwrap_or(0),
    }).collect())
}

#[tauri::command]
pub async fn delete_memory_rule(state: State<'_, AppState>, id: i64) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let vault_id = state.get_vault_id().await?;
    // Get rule id from list first
    let rows: Vec<serde_json::Value> = daemon_get(
        &state.http_client,
        &format!("/vaults/{}/memory/rules", urlencoding::encode(&vault_id)),
        tok,
    ).await.unwrap_or_default();
    if let Some(rule) = rows.into_iter().nth(id as usize) {
        if let Some(rule_id) = rule["id"].as_str() {
            daemon_delete::<serde_json::Value>(
                &state.http_client,
                &format!("/vaults/{}/memory/rules/{}", urlencoding::encode(&vault_id), urlencoding::encode(rule_id)),
                tok,
            ).await.map(|_| ()).map_err(|e| AppError::Database(e))?;
        }
    }
    Ok(())
}

// ─── Memory Session ────────────────────────────────────────────────────────────

/// Agent 工具：查詢記憶筆記（回傳格式化純文字，供 LLM 直接使用）
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

    let vault_id = state.get_vault_id().await?;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    let memories_dir = PathBuf::from(&vault_path).join("memories");
    tokio::fs::create_dir_all(&memories_dir).await
        .map_err(|e| AppError::Vault(format!("建立 memories 資料夾失敗：{}", e)))?;

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
            _ => {}
        }
    }

    let abs_path = PathBuf::from(&vault_path).join(&rel_path);
    tokio::fs::write(&abs_path, &content).await
        .map_err(|e| AppError::Vault(format!("寫入記憶筆記失敗：{}", e)))?;

    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
        &serde_json::json!({"path": rel_path, "title": title, "content": content}),
        tok,
    ).await.ok();

    Ok(rel_path)
}

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

// ─── Preference Distillation ──────────────────────────────────────────────────

/// 從最近對話記憶中蒸餾使用者偏好，寫入 preferences/user_prefs.md 並注入為 active skill
#[tauri::command]
pub async fn distill_preferences(state: State<'_, AppState>) -> Result<(), AppError> {
    let vault_id = match state.get_vault_id().await {
        Ok(vid) => vid,
        Err(_) => return Ok(()),
    };
    let vault_path = state.get_vault_path().await;

    let base_url = {
        let port = *state.llama_actual_port.lock().await;
        match port {
            Some(p) => format!("http://127.0.0.1:{}", p),
            None => return Ok(()),
        }
    };

    // Query recent memory notes from vault filesystem
    let memories_dir = std::path::PathBuf::from(&vault_path).join("memories");
    let mut memory_contents: Vec<String> = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&memories_dir).await {
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                paths.push(path);
            }
        }
        // Sort by filename descending (timestamps in filename)
        paths.sort_by(|a, b| b.cmp(a));
        for path in paths.into_iter().take(5) {
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                memory_contents.push(content.chars().take(2000).collect());
            }
        }
    }
    if memory_contents.is_empty() { return Ok(()); }
    let combined: String = memory_contents.join("\n\n---\n\n");

    let client = state.http_client.clone();
    let body = serde_json::json!({
        "messages": [
            {
                "role": "system",
                "content": "你是一個使用者偏好分析系統。從以下對話記憶中，提取使用者的：\n1. 工作習慣與偏好（如回答風格、語言偏好）\n2. 常見需求模式\n3. 個人背景資訊（如果有）\n4. 重要規則（使用者明確表達的規定）\n\n輸出格式：條列式，每條以「- 」開頭，簡潔描述。不超過 15 條。只輸出偏好列表，不要說明或解釋。"
            },
            {
                "role": "user",
                "content": format!("以下是最近的對話記憶，請提取使用者偏好：\n\n{}", combined)
            }
        ],
        "max_tokens": 512,
        "temperature": 0.3,
        "stream": false,
    });

    let http_resp = match client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return Ok(()),
    };

    let json: serde_json::Value = match http_resp.json().await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    let prefs = match json["choices"][0]["message"]["content"].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return Ok(()),
    };

    let now = Local::now();
    let content = format!(
        "---\ncreated: {}\n---\n\n# 使用者偏好（自動蒸餾）\n\n更新時間：{}\n\n{}\n",
        now.to_rfc3339(),
        now.format("%Y-%m-%d %H:%M"),
        prefs
    );

    if !vault_path.is_empty() {
        let prefs_dir = std::path::PathBuf::from(&vault_path).join("preferences");
        tokio::fs::create_dir_all(&prefs_dir).await.ok();
        let abs_path = prefs_dir.join("user_prefs.md");
        tokio::fs::write(&abs_path, &content).await.ok();
    }

    // Post to daemon so it can index the preferences note
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let rel_path = "preferences/user_prefs.md".to_string();
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
        &serde_json::json!({"path": rel_path, "title": "使用者偏好（自動蒸餾）", "content": content}),
        tok,
    ).await.ok();

    Ok(())
}

// ─── Response Ratings ─────────────────────────────────────────────────────────

/// 記錄使用者對回覆的評分（好/壞），供 analyze_tool_patterns 使用
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

/// 取得某對話所有回覆的評分記錄，供前端 UI 還原 👍/👎 狀態
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

// ─── Tool Pattern Analysis ────────────────────────────────────────────────────

/// 分析最近對話中工具呼叫評分，找出高評分模式並持久化為記憶規則
#[tauri::command]
pub async fn analyze_tool_patterns(state: State<'_, AppState>) -> Result<u32, AppError> {
    let vault_id = match state.get_vault_id().await {
        Ok(id) => id,
        Err(_) => return Ok(0),
    };
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    // Get recent ratings with "good" rating
    let rows: Vec<serde_json::Value> = daemon_get(
        &state.http_client,
        &format!("/vaults/{}/ratings", urlencoding::encode(&vault_id)),
        tok,
    ).await.unwrap_or_default();

    let good_count = rows.iter().filter(|r| r["rating"].as_str() == Some("good")).count() as u32;
    Ok(good_count)
}

// ─── Memory Fact Extraction ────────────────────────────────────────────────────

/// 從對話中萃取離散記憶事實，儲存為記憶筆記片段供未來查詢
#[tauri::command]
pub async fn extract_memory_facts(
    state: State<'_, AppState>,
    messages: Vec<ChatMessage>,
) -> Result<u32, AppError> {
    if messages.is_empty() { return Ok(0); }

    let base_url = {
        let port = *state.llama_actual_port.lock().await;
        match port {
            Some(p) => format!("http://127.0.0.1:{}", p),
            None => return Ok(0),
        }
    };

    // Build conversation text
    let conv_text: String = messages.iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| format!("[{}]: {}", m.role, &m.content.chars().take(500).collect::<String>()))
        .collect::<Vec<_>>().join("\n");

    let body = serde_json::json!({
        "messages": [
            {
                "role": "system",
                "content": "從以下對話中萃取 3-5 條具體的、可獨立理解的記憶事實。\n每條以「-」開頭，格式：「- [事實]」。只輸出事實列表，不需說明。",
            },
            { "role": "user", "content": conv_text },
        ],
        "stream": false,
        "temperature": 0.1,
        "max_tokens": 300,
    });

    let client = &state.http_client;
    let Ok(resp) = client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send().await
    else { return Ok(0); };

    let Ok(json) = resp.json::<serde_json::Value>().await else { return Ok(0); };
    let facts_text = json["choices"][0]["message"]["content"].as_str().unwrap_or("");
    let facts: Vec<&str> = facts_text.lines()
        .filter(|l| l.trim().starts_with('-'))
        .collect();
    let count = facts.len() as u32;

    if count > 0 {
        // Append facts as a note fragment to the latest memory session if vault is set
        let vault_path = state.get_vault_path().await;
        if !vault_path.is_empty() {
            let now = Local::now();
            let timestamp = now.format("%Y%m%d_%H%M%S").to_string();
            let rel_path = format!("memories/facts_{}.md", timestamp);
            let content = format!(
                "---\ncreated: {}\ntags: [memory-facts]\n---\n\n# 記憶事實片段\n\n{}\n",
                now.to_rfc3339(), facts_text
            );
            let abs_path = PathBuf::from(&vault_path).join(&rel_path);
            tokio::fs::create_dir_all(abs_path.parent().unwrap_or(&abs_path)).await.ok();
            tokio::fs::write(&abs_path, &content).await.ok();
            // Sync to daemon (no file watcher in daemon)
            if let Ok(vid) = state.get_vault_id().await {
                if !vid.is_empty() {
                    let token2 = state.get_auth_token().await;
                    let tok2: Option<&str> = if token2.is_empty() { None } else { Some(token2.as_str()) };
                    daemon_post::<_, serde_json::Value>(
                        &state.http_client,
                        &format!("/vaults/{}/notes", urlencoding::encode(&vid)),
                        &serde_json::json!({"path": rel_path, "content": content}),
                        tok2,
                    ).await.ok();
                }
            }
        }
    }

    Ok(count)
}

// ─── Retrieval ────────────────────────────────────────────────────────────────

/// Retrieve relevant memory facts from daemon memory query.
pub(crate) async fn retrieve_relevant_facts(
    vault_id: &str,
    query_text: &str,
    _query_embedding: &[f32],
    limit: usize,
) -> String {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let url = format!(
        "http://127.0.0.1:7787/api/v1/vaults/{}/memory/query?keywords={}&limit={}",
        urlencoding::encode(vault_id),
        urlencoding::encode(query_text),
        limit,
    );
    let Ok(resp) = client.get(&url).send().await else { return String::new(); };
    let Ok(json) = resp.json::<serde_json::Value>().await else { return String::new(); };
    let arr = json.as_array().cloned().unwrap_or_default();
    if arr.is_empty() { return String::new(); }
    arr.iter().filter_map(|r| {
        let snippet = r["snippet"].as_str()?;
        Some(snippet.to_string())
    }).collect::<Vec<_>>().join("\n")
}

fn extract_cjk_keywords(text: &str, max: usize) -> Vec<String> {
    const STOPS: &[char] = &[
        '你','我','他','她','它','的','了','嗎','是','有','在','說','道','記',
        '什','麼','這','那','就','都','也','還','不','沒','要','會','可','以',
        '和','與','或','但','如','果','因','為','所','而','且','呢','嗎','啊',
    ];
    let chars: Vec<char> = text.chars()
        .filter(|c| c.is_alphanumeric() && !STOPS.contains(c))
        .collect();
    let mut bigrams = Vec::new();
    for w in chars.windows(2) {
        if w[0] as u32 > 0x4E00 && w[1] as u32 > 0x4E00 {
            bigrams.push(format!("{}{}", w[0], w[1]));
        }
    }
    bigrams.dedup();
    bigrams.truncate(max);
    bigrams
}
