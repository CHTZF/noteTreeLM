use crate::{error::AppError, state::AppState};
use chrono::Local;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tauri::State;

use super::ai::ChatMessage;
use super::server::get_embedding;

// ─── Memory Rules ─────────────────────────────────────────────────────────────

/// 讓前端或其他命令可以直接向資料庫新增記憶規則
#[tauri::command]
pub async fn add_memory_rule(
    state: State<'_, AppState>,
    pattern_type: String,
    pattern: String,
    value: String,
) -> Result<(), AppError> {
    let db = &state.db;
    let vault_id = state.get_vault_id().await?;
    db.query(
        "INSERT INTO memory_rules (vault_id, pattern_type, pattern, value) VALUES ($vid, $pt, $p, $v)
         ON DUPLICATE KEY UPDATE value = $v"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("pt", pattern_type.clone()))
    .bind(("p", pattern.clone()))
    .bind(("v", value.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
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
    let db = &state.db;
    let vault_id = state.get_vault_id().await?;

    #[derive(Deserialize)]
    struct RuleRow {
        pattern_type: String,
        pattern: String,
        value: String,
        created_at: surrealdb::sql::Datetime,
    }
    let mut resp = db.query(
        "SELECT pattern_type, pattern, value, created_at FROM memory_rules WHERE vault_id = $vid ORDER BY created_at"
    )
    .bind(("vid", vault_id.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<RuleRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    Ok(rows.into_iter().enumerate().map(|(idx, r)| {
        MemoryRuleEntry {
            id: idx as i64,
            pattern_type: r.pattern_type,
            pattern: r.pattern.clone(),
            value: r.value,
            created_at: r.created_at.timestamp(),
        }
    }).collect())
}

/// 刪除指定 pattern 的記憶規則（SurrealDB 版）
/// id 為前端傳入的 enumerate index（與 get_memory_rules 的 idx 對應）
#[tauri::command]
pub async fn delete_memory_rule(state: State<'_, AppState>, id: i64) -> Result<(), AppError> {
    let db = &state.db;
    let vault_id = state.get_vault_id().await?;

    #[derive(Deserialize)]
    struct PatternRow { pattern: String }
    let mut resp = db.query(
        "SELECT pattern FROM memory_rules WHERE vault_id = $vid ORDER BY created_at"
    )
    .bind(("vid", vault_id.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<PatternRow> = resp.take(0).unwrap_or_default();

    if let Some(row) = rows.into_iter().nth(id as usize) {
        db.query("DELETE memory_rules WHERE vault_id = $vid AND pattern = $pat")
            .bind(("vid", vault_id))
            .bind(("pat", row.pattern))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
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

    let db = &state.db;
    let vault_id = state.get_vault_id().await?;

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

    let now_dt = surrealdb::sql::Datetime::from(chrono::Utc::now());
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let checksum = format!("{:x}", hasher.finalize());
    let word_count = content.split_whitespace().count() as i64;

    db.query(
        "INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at, checksum)
         VALUES ($vid, $path, $title, $content, $wc, $now, $now, $checksum)
         ON DUPLICATE KEY UPDATE title = $title, content = $content, word_count = $wc, modified_at = $now, checksum = $checksum"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("path", rel_path.clone()))
    .bind(("title", title.clone()))
    .bind(("content", content.clone()))
    .bind(("wc", word_count))
    .bind(("now", now_dt))
    .bind(("checksum", checksum.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

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
    let limit = limit.unwrap_or(10).min(50) as i64;
    let db = &state.db;
    let vault_id = state.get_vault_id().await?;

    #[derive(Deserialize)]
    struct MemRow {
        path: String,
        title: String,
        created_at: surrealdb::sql::Datetime,
        content: String,
    }

    if keywords.is_empty() {
        let mut resp = db.query(
            "SELECT path, title, created_at, content FROM notes
             WHERE vault_id = $vid AND string::starts_with(path, 'memories/')
             ORDER BY created_at DESC
             LIMIT $limit"
        )
        .bind(("vid", vault_id.clone()))
        .bind(("limit", limit))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<MemRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

        return Ok(rows.into_iter().map(|r| {
            let snippet = r.content.chars().skip_while(|c| *c == '-' || *c == '\n').take(200).collect();
            MemoryResult { path: r.path, title: r.title, created_at: r.created_at.timestamp() * 1000, snippet }
        }).collect());
    }

    let fts_query = keywords.join(" OR ");
    let mut resp = db.query(
        "SELECT path, title, created_at, content FROM notes
         WHERE vault_id = $vid AND string::starts_with(path, 'memories/') AND content @1@ $q
         ORDER BY search::score(1) DESC
         LIMIT $limit"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("q", fts_query.clone()))
    .bind(("limit", limit))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<MemRow> = resp.take(0).unwrap_or_default();

    let since_ts: Option<i64> = since.and_then(|s| {
        chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok().map(|d| {
            d.and_hms_opt(0, 0, 0)
                .and_then(|dt| dt.and_local_timezone(Local).earliest())
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(0)
        })
    });

    let results = rows.into_iter()
        .filter(|r| since_ts.map_or(true, |min_ts| r.created_at.timestamp() * 1000 >= min_ts))
        .map(|r| {
            let snippet = r.content.chars().skip_while(|c| *c == '-' || *c == '\n').take(200).collect();
            MemoryResult { path: r.path, title: r.title, created_at: r.created_at.timestamp() * 1000, snippet }
        })
        .collect();

    Ok(results)
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
    let vault_db = state.db.clone();

    let base_url = {
        let port = *state.llama_actual_port.lock().await;
        match port {
            Some(p) => format!("http://127.0.0.1:{}", p),
            None => return Ok(()),
        }
    };

    #[derive(Deserialize)]
    struct MemRow { content: String }
    let mut resp = match vault_db.query(
        "SELECT content FROM notes \
         WHERE vault_id = $vid AND string::starts_with(path, 'memories/') \
         ORDER BY modified_at DESC LIMIT 5"
    )
    .bind(("vid", vault_id.clone()))
    .await {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };

    let rows: Vec<MemRow> = resp.take(0).unwrap_or_default();
    if rows.is_empty() { return Ok(()); }

    let combined: String = rows.iter()
        .map(|r| r.content.chars().take(2000).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");

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
    let rel_path = "preferences/user_prefs.md".to_string();

    if !vault_path.is_empty() {
        let prefs_dir = std::path::PathBuf::from(&vault_path).join("preferences");
        tokio::fs::create_dir_all(&prefs_dir).await.ok();
        let abs_path = prefs_dir.join("user_prefs.md");
        tokio::fs::write(&abs_path, &content).await.ok();
    }

    let now_dt = surrealdb::sql::Datetime::from(chrono::Utc::now());
    let word_count = content.split_whitespace().count() as i64;
    let title = "使用者偏好（自動蒸餾）".to_string();

    let _ = vault_db.query(
        "INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at, checksum)
         VALUES ($vid, $path, $title, $content, $wc, $now, $now, '')
         ON DUPLICATE KEY UPDATE title = $title, content = $content, word_count = $wc, modified_at = $now"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("path", rel_path))
    .bind(("title", title.clone()))
    .bind(("content", content.clone()))
    .bind(("wc", word_count))
    .bind(("now", now_dt.clone()))
    .await;

    let skill_id = "__user_prefs__".to_string();
    let _ = vault_db.query(
        "DELETE FROM agent_skills WHERE vault_id = $vid AND skill_id = $sid"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("sid", skill_id.clone()))
    .await;

    let _ = vault_db.query(
        "INSERT INTO agent_skills \
         (skill_id, vault_id, title, trigger, behavior, auto_tool_calls, \
          is_active, injection_mode, trigger_count, created_at) \
         VALUES ($sid, $vid, $title, $trigger, $behavior, [], true, 'active', 0, $now)"
    )
    .bind(("sid", skill_id))
    .bind(("vid", vault_id))
    .bind(("title", title))
    .bind(("trigger", "每次對話".to_string()))
    .bind(("behavior", prefs))
    .bind(("now", now_dt))
    .await;

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
    if !matches!(rating.as_str(), "good" | "bad") { return Ok(()); }
    let vault_id = match state.get_vault_id().await {
        Ok(vid) => vid,
        Err(_) => return Ok(()),
    };
    let now_dt = surrealdb::sql::Datetime::from(chrono::Utc::now());
    let id = uuid::Uuid::new_v4().to_string();
    let conv_id = conversation_id.unwrap_or_default();

    let _ = state.db.query(
        "INSERT INTO response_feedback \
         (id, vault_id, conversation_id, content_hash, rating, created_at) \
         VALUES ($id, $vid, $conv, $hash, $rating, $now)"
    )
    .bind(("id", id))
    .bind(("vid", vault_id))
    .bind(("conv", conv_id))
    .bind(("hash", content_hash))
    .bind(("rating", rating))
    .bind(("now", now_dt))
    .await;

    Ok(())
}

/// 取得某對話所有回覆的評分記錄，供前端 UI 還原 👍/👎 狀態
#[tauri::command]
pub async fn get_conversation_ratings(
    state: State<'_, AppState>,
    conversation_id: String,
) -> Result<Vec<serde_json::Value>, AppError> {
    let vault_id = match state.get_vault_id().await {
        Ok(vid) => vid,
        Err(_) => return Ok(vec![]),
    };

    #[derive(Deserialize)]
    struct FeedbackRow { content_hash: String, rating: String }

    let mut resp = state.db.query(
        "SELECT content_hash, rating FROM response_feedback \
         WHERE vault_id = $vid AND conversation_id = $conv \
         ORDER BY created_at ASC"
    )
    .bind(("vid", vault_id))
    .bind(("conv", conversation_id))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    let rows: Vec<FeedbackRow> = resp.take(0).unwrap_or_default();
    Ok(rows.into_iter().map(|r| serde_json::json!({
        "content_hash": r.content_hash,
        "rating": r.rating,
    })).collect())
}

// ─── Tool Pattern Analysis ────────────────────────────────────────────────────

/// 分析最近對話中的工具呼叫序列，自動生成/更新技能規範（auto_tool_calls）
#[tauri::command]
pub async fn analyze_tool_patterns(state: State<'_, AppState>) -> Result<u32, AppError> {
    let vault_id = match state.get_vault_id().await {
        Ok(vid) => vid,
        Err(_) => return Ok(0),
    };
    let vault_db = state.db.clone();

    let base_url = {
        let port = *state.llama_actual_port.lock().await;
        match port {
            Some(p) => format!("http://127.0.0.1:{}", p),
            None => return Ok(0),
        }
    };

    #[derive(Deserialize)]
    struct ConvRow { id: String, messages_json: String }
    let mut resp = match vault_db.query(
        "SELECT record::id(id) AS id, messages_json FROM conversations ORDER BY updated_at DESC LIMIT 30"
    ).await {
        Ok(r) => r,
        Err(_) => return Ok(0),
    };
    let rows: Vec<ConvRow> = resp.take(0).unwrap_or_default();

    #[derive(Deserialize)]
    struct FeedbackRow { conversation_id: String }
    let bad_convs: std::collections::HashSet<String> = {
        let mut fr = vault_db.query(
            "SELECT conversation_id FROM response_feedback \
             WHERE vault_id = $vid AND rating = 'bad'"
        )
        .bind(("vid", vault_id.clone()))
        .await.ok();
        let frows: Vec<FeedbackRow> = fr.as_mut()
            .and_then(|r| r.take::<Vec<FeedbackRow>>(0).ok())
            .unwrap_or_default();
        frows.into_iter().map(|r| r.conversation_id).collect()
    };

    if rows.is_empty() { return Ok(0); }

    let mut sequence_counts: std::collections::HashMap<Vec<String>, u32> =
        std::collections::HashMap::new();

    for row in rows.iter().filter(|r| !bad_convs.contains(&r.id)) {
        let Ok(msgs) = serde_json::from_str::<serde_json::Value>(&row.messages_json) else { continue };
        let Some(arr) = msgs.as_array() else { continue };

        let mut seq: Vec<String> = Vec::new();
        let mut last_user_idx: Option<usize> = None;

        for (i, msg) in arr.iter().enumerate() {
            match msg["role"].as_str() {
                Some("user") => {
                    if seq.len() >= 2 {
                        *sequence_counts.entry(seq.clone()).or_insert(0) += 1;
                    }
                    seq.clear();
                    last_user_idx = Some(i);
                }
                Some("assistant") => {
                    if let Some(tool_calls) = msg["tool_calls"].as_array() {
                        for tc in tool_calls {
                            if let Some(name) = tc["function"]["name"].as_str() {
                                if !name.starts_with("__") {
                                    seq.push(name.to_string());
                                }
                            }
                        }
                    }
                    let _ = last_user_idx;
                }
                _ => {}
            }
        }
        if seq.len() >= 2 {
            *sequence_counts.entry(seq).or_insert(0) += 1;
        }
    }

    let frequent: Vec<(Vec<String>, u32)> = sequence_counts
        .into_iter()
        .filter(|(seq, count)| *count >= 2 && seq.len() >= 2 && seq.len() <= 5)
        .collect();

    if frequent.is_empty() { return Ok(0); }

    let mut summary = String::new();
    for (seq, count) in &frequent {
        summary.push_str(&format!("- {} 次：{}\n", count, seq.join(" → ")));
    }

    let client = state.http_client.clone();
    let prompt = format!(
        "以下是從對話記錄中統計出的工具呼叫序列（格式：次數：tool1 → tool2 → ...）：\n\n{}\n\n\
請為每個序列輸出 JSON 陣列，每個元素包含：\n\
- trigger: 觸發此序列的使用者意圖（以「當使用者...時」開頭，15字以內）\n\
- first_tool: 序列中第一個工具名稱（與輸入完全一致）\n\
- behavior: 操作說明（20字以內，描述先做什麼再做什麼）\n\n\
只輸出 JSON 陣列，不要任何說明。範例：\n\
[{{\"trigger\":\"當使用者要打開筆記時\",\"first_tool\":\"search_vault\",\"behavior\":\"先 search_vault 找路徑，再 open_note\"}}]",
        summary
    );

    let body = serde_json::json!({
        "messages": [
            {"role": "user", "content": prompt}
        ],
        "max_tokens": 600,
        "temperature": 0.2,
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
        _ => return Ok(0),
    };

    let json: serde_json::Value = match http_resp.json().await {
        Ok(v) => v,
        Err(_) => return Ok(0),
    };

    let text = json["choices"][0]["message"]["content"].as_str().unwrap_or("");
    let start = match text.find('[') { Some(i) => i, None => return Ok(0) };
    let end   = match text.rfind(']') { Some(i) => i + 1, None => return Ok(0) };
    let patterns: serde_json::Value = match serde_json::from_str(&text[start..end]) {
        Ok(v) => v,
        Err(_) => return Ok(0),
    };

    let arr = match patterns.as_array() {
        Some(a) => a,
        None => return Ok(0),
    };

    let mut created = 0u32;
    let now_dt = surrealdb::sql::Datetime::from(chrono::Utc::now());

    for item in arr {
        let trigger = item["trigger"].as_str().unwrap_or("").to_string();
        let first_tool = item["first_tool"].as_str().unwrap_or("").to_string();
        let behavior = item["behavior"].as_str().unwrap_or("").to_string();

        if trigger.is_empty() || first_tool.is_empty() || behavior.is_empty() { continue; }

        let skill_id = format!("__pattern__{:x}",
            trigger.bytes().fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64)));

        #[derive(Deserialize)]
        struct ExistRow { skill_id: String }
        let mut check = vault_db.query(
            "SELECT skill_id FROM agent_skills WHERE vault_id = $vid AND skill_id = $sid LIMIT 1"
        )
        .bind(("vid", vault_id.clone()))
        .bind(("sid", skill_id.clone()))
        .await.ok();
        let exists: Vec<ExistRow> = check.as_mut()
            .and_then(|r| r.take::<Vec<ExistRow>>(0).ok())
            .unwrap_or_default();
        if !exists.is_empty() { continue; }

        let title = format!("【自動】{}", trigger.trim_start_matches("當使用者").trim_start_matches("當"));
        let auto_tools = vec![first_tool];

        let _ = vault_db.query(
            "INSERT INTO agent_skills \
             (skill_id, vault_id, title, trigger, behavior, auto_tool_calls, \
              is_active, injection_mode, trigger_count, created_at) \
             VALUES ($sid, $vid, $title, $trigger, $behavior, $tools, \
                     false, 'passive', 0, $now)"
        )
        .bind(("sid", skill_id))
        .bind(("vid", vault_id.clone()))
        .bind(("title", title))
        .bind(("trigger", trigger))
        .bind(("behavior", behavior))
        .bind(("tools", auto_tools))
        .bind(("now", now_dt.clone()))
        .await;

        created += 1;
    }

    Ok(created)
}

// ─── Memory Fact Extraction ────────────────────────────────────────────────────

/// 從對話中萃取離散記憶事實，embedding 去重後 upsert 至 memory_facts 表
#[tauri::command]
pub async fn extract_memory_facts(
    state: State<'_, AppState>,
    messages: Vec<ChatMessage>,
) -> Result<u32, AppError> {
    let vault_id = match state.get_vault_id().await {
        Ok(vid) => vid,
        Err(_) => return Ok(0),
    };
    let vault_db = state.db.clone();

    let base_url = {
        let port = *state.llama_actual_port.lock().await;
        match port {
            Some(p) => format!("http://127.0.0.1:{}", p),
            None => return Ok(0),
        }
    };

    let dialog: String = messages.iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| {
            let (prefix, limit) = if m.role == "user" {
                ("使用者", 300usize)
            } else {
                ("助理", 1000usize)
            };
            format!("{}: {}", prefix, m.content.chars().take(limit).collect::<String>())
        })
        .collect::<Vec<_>>()
        .join("\n");

    if dialog.is_empty() { return Ok(0); }

    let client = state.http_client.clone();
    let prompt = format!(
        "你是記憶萃取系統。從以下對話中提取 3-10 條值得長期記憶的離散事實。\n\
規則：\n\
- 每條事實必須獨立可理解，不依賴對話上下文\n\
- 只提取有長期價值的資訊：偏好、背景、規則、重要知識\n\
- 忽略一次性問答、閒聊、具體指令執行結果\n\
- 每條 10-30 字，簡潔精確\n\
- category 只能選：preference（偏好）、context（背景）、knowledge（知識）、rule（規則）\n\n\
輸出純 JSON 陣列，不要說明：\n\
[{{\"category\":\"...\",\"content\":\"...\"}}]\n\n\
對話：\n{}",
        dialog
    );

    let body = serde_json::json!({
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 512,
        "temperature": 0.2,
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
        _ => return Ok(0),
    };

    let json: serde_json::Value = match http_resp.json().await {
        Ok(v) => v,
        Err(_) => return Ok(0),
    };

    let text = json["choices"][0]["message"]["content"].as_str().unwrap_or("");
    let start = match text.find('[') { Some(i) => i, None => return Ok(0) };
    let end   = match text.rfind(']') { Some(i) => i + 1, None => return Ok(0) };
    let facts_val: serde_json::Value = match serde_json::from_str(&text[start..end]) {
        Ok(v) => v,
        Err(_) => return Ok(0),
    };
    let facts = match facts_val.as_array() {
        Some(a) => a.clone(),
        None => return Ok(0),
    };

    let emb_url: Option<String> = {
        let port = *state.embedding_actual_port.lock().await;
        port.map(|p| format!("http://127.0.0.1:{}", p))
    };

    let parsed: Vec<(String, String)> = facts.iter().filter_map(|fact| {
        let content = fact["content"].as_str().filter(|s| !s.is_empty())?.to_string();
        let category = fact["category"].as_str()
            .filter(|c| matches!(*c, "preference"|"context"|"knowledge"|"rule"))
            .unwrap_or("knowledge")
            .to_string();
        Some((content, category))
    }).collect();

    let embeddings: Vec<Vec<f32>> = if let Some(ref eu) = emb_url {
        let futs: Vec<_> = parsed.iter()
            .map(|(content, _)| get_embedding(&client, eu, content))
            .collect();
        futures::future::join_all(futs).await
    } else {
        vec![vec![]; parsed.len()]
    };

    let mut inserted = 0u32;
    let now_dt = surrealdb::sql::Datetime::from(chrono::Utc::now());

    for ((content, category), fact_embedding) in parsed.iter().zip(embeddings.iter()) {
        let existing_id: Option<String> = if !fact_embedding.is_empty() {
            #[derive(Deserialize)]
            struct SimilarRow { fact_id: String }
            let qvec: Vec<f64> = fact_embedding.iter().map(|&x| x as f64).collect();
            let mut resp = vault_db.query(
                "SELECT fact_id FROM memory_facts \
                 WHERE vault_id = $vid AND embedding IS NOT NONE \
                   AND vector::similarity::cosine(embedding, $vec) > 0.88 \
                 LIMIT 1"
            )
            .bind(("vid", vault_id.clone()))
            .bind(("vec", qvec))
            .await.ok();
            resp.as_mut()
                .and_then(|r| r.take::<Vec<SimilarRow>>(0).ok())
                .and_then(|rows| rows.into_iter().next())
                .map(|r| r.fact_id)
        } else {
            None
        };

        let emb_val: serde_json::Value = if !fact_embedding.is_empty() {
            let v: Vec<f64> = fact_embedding.iter().map(|&x| x as f64).collect();
            serde_json::json!(v)
        } else {
            serde_json::Value::Null
        };

        if let Some(fid) = existing_id {
            let _ = vault_db.query(
                "UPDATE memory_facts SET content = $content, category = $cat, \
                                        embedding = $emb, updated_at = $now \
                 WHERE fact_id = $fid"
            )
            .bind(("fid", fid))
            .bind(("content", content.clone()))
            .bind(("cat", category.clone()))
            .bind(("emb", emb_val))
            .bind(("now", now_dt.clone()))
            .await;
        } else {
            let fact_id = uuid::Uuid::new_v4().to_string();
            let _ = vault_db.query(
                "INSERT INTO memory_facts \
                 (fact_id, vault_id, content, category, embedding, \
                  access_count, created_at, updated_at) \
                 VALUES ($fid, $vid, $content, $cat, $emb, 0, $now, $now)"
            )
            .bind(("fid", fact_id))
            .bind(("vid", vault_id.clone()))
            .bind(("content", content.clone()))
            .bind(("cat", category.clone()))
            .bind(("emb", emb_val))
            .bind(("now", now_dt.clone()))
            .await;
            inserted += 1;
        }
    }

    // LRU eviction：超過 500 條時刪 access_count 最低且最舊的多餘事實
    const MAX_FACTS: i64 = 500;
    {
        #[derive(Deserialize)]
        struct CntRow { cnt: i64 }
        let mut cr = vault_db.query(
            "SELECT count() AS cnt FROM memory_facts WHERE vault_id = $vid GROUP ALL"
        )
        .bind(("vid", vault_id.clone()))
        .await.ok();
        let total: i64 = cr.as_mut()
            .and_then(|r| r.take::<Vec<CntRow>>(0).ok())
            .and_then(|v| v.into_iter().next())
            .map(|r| r.cnt)
            .unwrap_or(0);

        if total > MAX_FACTS {
            let excess = total - MAX_FACTS;
            let _ = vault_db.query(
                "DELETE memory_facts WHERE id IN (\
                   SELECT id FROM memory_facts WHERE vault_id = $vid \
                   ORDER BY access_count ASC, updated_at ASC \
                   LIMIT $excess\
                 )"
            )
            .bind(("vid", vault_id.clone()))
            .bind(("excess", excess))
            .await;
        }
    }

    Ok(inserted)
}

// ─── Retrieval ────────────────────────────────────────────────────────────────

pub(crate) async fn retrieve_relevant_facts(
    db: &crate::db::surreal::SurrealDb,
    vault_id: &str,
    query_text: &str,
    query_embedding: &[f32],
    limit: usize,
) -> String {
    // 空表快速返回（避免對新用戶發出無謂的 embedding 呼叫）
    {
        #[derive(serde::Deserialize)]
        struct CntRow { cnt: i64 }
        let mut cr = db.query(
            "SELECT count() AS cnt FROM memory_facts WHERE vault_id = $vid GROUP ALL"
        )
        .bind(("vid", vault_id.to_string()))
        .await.ok();
        let n: i64 = cr.as_mut()
            .and_then(|r| r.take::<Vec<CntRow>>(0).ok())
            .and_then(|v| v.into_iter().next())
            .map(|r| r.cnt)
            .unwrap_or(0);
        if n == 0 { return String::new(); }
    }

    #[derive(serde::Deserialize)]
    struct FactRow {
        content: String,
        category: String,
    }

    let limit_i = limit as i64;

    // 向量搜尋路徑
    if !query_embedding.is_empty() {
        let qvec: Vec<f64> = query_embedding.iter().map(|&x| x as f64).collect();
        let mut resp = db.query(
            "SELECT content, category, \
                    vector::similarity::cosine(embedding, $vec) AS score \
             FROM memory_facts \
             WHERE vault_id = $vid AND embedding IS NOT NONE \
             ORDER BY score DESC LIMIT $lim"
        )
        .bind(("vid", vault_id.to_string()))
        .bind(("vec", qvec))
        .bind(("lim", limit_i))
        .await.ok();

        let rows: Vec<FactRow> = resp.as_mut()
            .and_then(|r| r.take::<Vec<FactRow>>(0).ok())
            .unwrap_or_default();

        if !rows.is_empty() {
            let returned_contents: Vec<String> = rows.iter().map(|r| r.content.clone()).collect();
            let _ = db.query(
                "UPDATE memory_facts SET access_count += 1, last_accessed_at = time::now() \
                 WHERE vault_id = $vid AND content IN $contents"
            )
            .bind(("vid", vault_id.to_string()))
            .bind(("contents", returned_contents))
            .await;

            let lines: Vec<String> = rows.iter()
                .map(|r| format!("• [{}] {}", r.category, r.content))
                .collect();
            return format!(
                "[記憶] （共 {} 條，依語意相關性篩選）\n{}",
                lines.len(),
                lines.join("\n")
            );
        }
    }

    // Fallback 1：CJK bigram keyword 搜尋（無 embedding server 時仍保有相關性）
    let keywords = extract_cjk_keywords(query_text, 3);
    if !keywords.is_empty() {
        let kw = &keywords[0];
        let mut resp = db.query(
            "SELECT content, category FROM memory_facts \
             WHERE vault_id = $vid AND string::contains(content, $kw) \
             ORDER BY access_count DESC LIMIT $lim"
        )
        .bind(("vid", vault_id.to_string()))
        .bind(("kw", kw.clone()))
        .bind(("lim", limit_i))
        .await.ok();

        let rows: Vec<FactRow> = resp.as_mut()
            .and_then(|r| r.take::<Vec<FactRow>>(0).ok())
            .unwrap_or_default();

        if !rows.is_empty() {
            let lines: Vec<String> = rows.iter()
                .map(|r| format!("• [{}] {}", r.category, r.content))
                .collect();
            return format!(
                "[記憶] （共 {} 條，依關鍵字相關性篩選）\n{}",
                lines.len(),
                lines.join("\n")
            );
        }
    }

    // Fallback 2：純最新事實（無任何 embedding 也無關鍵字命中時）
    let mut resp = db.query(
        "SELECT content, category FROM memory_facts \
         WHERE vault_id = $vid \
         ORDER BY created_at DESC LIMIT $lim"
    )
    .bind(("vid", vault_id.to_string()))
    .bind(("lim", limit_i))
    .await.ok();

    let rows: Vec<FactRow> = resp.as_mut()
        .and_then(|r| r.take::<Vec<FactRow>>(0).ok())
        .unwrap_or_default();

    if rows.is_empty() {
        return String::new();
    }

    let lines: Vec<String> = rows.iter()
        .map(|r| format!("• [{}] {}", r.category, r.content))
        .collect();
    format!(
        "[記憶] （共 {} 條，最新優先）\n{}",
        lines.len(),
        lines.join("\n")
    )
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
