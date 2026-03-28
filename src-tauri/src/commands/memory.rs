use crate::{api_client::{daemon_delete, daemon_get, daemon_post}, error::AppError, state::AppState};
use chrono::Local;
use serde::Serialize;
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

/// 從 memory_facts 蒸餾使用者偏好，upsert 為特殊 agent_skill（pure DB，不寫 disk）
#[tauri::command]
pub async fn distill_preferences(state: State<'_, AppState>) -> Result<(), AppError> {
    let vault_id = match state.get_vault_id().await {
        Ok(vid) => vid,
        Err(_) => return Ok(()),
    };

    let base_url = {
        let port = *state.llama_actual_port.lock().await;
        match port {
            Some(p) => format!("http://127.0.0.1:{}", p),
            None => return Ok(()),
        }
    };

    let token = state.get_auth_token().await;
    let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };

    // Read recent personal/preference/rule facts from daemon
    let url = format!(
        "/vaults/{}/memory/query?category=personal,preference,rule&limit=30",
        urlencoding::encode(&vault_id)
    );
    let facts: Vec<serde_json::Value> = daemon_get(&state.http_client, &url, tok)
        .await.unwrap_or_default();

    if facts.is_empty() { return Ok(()); }

    let combined = facts.iter()
        .filter_map(|f| {
            let cat = f["category"].as_str().unwrap_or("general");
            let content = f["content"].as_str()?;
            Some(format!("[{}] {}", cat, content))
        })
        .collect::<Vec<_>>().join("\n");

    let body = serde_json::json!({
        "messages": [
            {
                "role": "system",
                "content": "你是使用者偏好分析系統。從以下記憶事實中，整理出使用者的偏好規則。\n輸出格式：條列式，每條以「- 」開頭，簡潔描述。不超過 15 條。只輸出列表，不要說明。"
            },
            {
                "role": "user",
                "content": format!("記憶事實：\n{}", combined)
            }
        ],
        "max_tokens": 512,
        "temperature": 0.3,
        "stream": false,
    });

    let http_resp = match state.http_client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send().await
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
    let behavior = format!(
        "更新時間：{}\n\n{}", now.format("%Y-%m-%d %H:%M"), prefs
    );

    // Upsert as a special agent_skill so it gets auto-injected as context
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/skills", urlencoding::encode(&vault_id)),
        &serde_json::json!({
            "skill_id": "distilled_user_prefs",
            "title": "使用者偏好（自動蒸餾）",
            "trigger": "__auto_injected__",
            "behavior": behavior,
            "is_active": true,
            "injection_mode": "system",
        }),
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

/// 從對話中萃取帶分類的記憶事實，存入 memory_facts DB（不寫 disk）
#[tauri::command]
pub async fn extract_memory_facts(
    state: State<'_, AppState>,
    messages: Vec<ChatMessage>,
    conversation_id: Option<String>,
) -> Result<u32, AppError> {
    if messages.is_empty() { return Ok(0); }

    let base_url = {
        let port = *state.llama_actual_port.lock().await;
        match port {
            Some(p) => format!("http://127.0.0.1:{}", p),
            None => return Ok(0),
        }
    };

    let vault_id = match state.get_vault_id().await {
        Ok(v) if !v.is_empty() => v,
        _ => return Ok(0),
    };
    let token = state.get_auth_token().await;
    let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };

    // Fetch existing recent facts for dedup context (pass to LLM)
    let existing_url = format!(
        "/vaults/{}/memory/query?limit=20",
        urlencoding::encode(&vault_id)
    );
    let existing_facts: Vec<serde_json::Value> = daemon_get(
        &state.http_client, &existing_url, tok,
    ).await.unwrap_or_default();
    let existing_summary = existing_facts.iter()
        .filter_map(|f| {
            let cat = f["category"].as_str().unwrap_or("general");
            let content = f["content"].as_str()?;
            Some(format!("[{}] {}", cat, content))
        })
        .collect::<Vec<_>>().join("\n");

    // Build conversation text (truncate each message)
    let conv_text: String = messages.iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| format!("[{}]: {}", m.role, m.content.chars().take(500).collect::<String>()))
        .collect::<Vec<_>>().join("\n");

    let existing_section = if existing_summary.is_empty() {
        String::new()
    } else {
        format!("\n\n已知事實（不要重複）：\n{}", existing_summary)
    };

    let system_prompt = format!(
        "從以下對話中萃取 3-5 條具體記憶事實。每條格式：`[category] 事實內容`\n\
         category 必須是以下其中一個：\n\
         - personal（個人資訊：姓名、地點、職業等）\n\
         - preference（偏好風格：回答方式、語言、習慣等）\n\
         - project（進行中專案或工作）\n\
         - rule（使用者明確要求的規則）\n\
         - general（其他值得記住的事實）\n\
         {}\n\n只輸出事實列表，每行一條，不需說明或編號。",
        existing_section
    );

    let body = serde_json::json!({
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": conv_text },
        ],
        "stream": false,
        "temperature": 0.1,
        "max_tokens": 400,
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

    // Parse `[category] content` lines
    let facts: Vec<serde_json::Value> = facts_text.lines()
        .filter_map(|line| {
            let line = line.trim();
            // Accept lines starting with optional "- " then "[category]"
            let line = line.trim_start_matches("- ").trim_start_matches("* ");
            if !line.starts_with('[') { return None; }
            let close = line.find(']')?;
            let category = line[1..close].trim().to_lowercase();
            let valid_cats = ["personal", "preference", "project", "rule", "general"];
            let cat = if valid_cats.contains(&category.as_str()) {
                category
            } else {
                "general".to_string()
            };
            let content = line[close + 1..].trim().to_string();
            if content.is_empty() { return None; }
            Some(serde_json::json!({
                "content": content,
                "category": cat,
                "conv_id": conversation_id,
            }))
        })
        .collect();

    let count = facts.len() as u32;
    if count == 0 { return Ok(0); }

    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/memory/facts", urlencoding::encode(&vault_id)),
        &serde_json::json!({ "facts": facts }),
        tok,
    ).await.ok();

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

// ─── Activity Pattern → Skill Suggestion ──────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SkillSuggestion {
    pub signature: String,
    pub score: f64,
    pub intent: String,
    pub suggested_title: String,
    pub suggested_trigger: String,
    pub suggested_behavior: String,
}

/// 從高分 activity_patterns 自動建議可轉成 skill 的候選，
/// 用 LLM 生成 title/trigger/behavior，回傳給前端顯示通知。
#[tauri::command]
pub async fn suggest_skills_from_patterns(state: State<'_, AppState>) -> Result<Vec<SkillSuggestion>, AppError> {
    let vault_id = match state.get_vault_id().await {
        Ok(v) if !v.is_empty() => v,
        _ => return Ok(vec![]),
    };
    let base_url = {
        let port = *state.llama_actual_port.lock().await;
        match port {
            Some(p) => format!("http://127.0.0.1:{}", p),
            None => return Ok(vec![]),
        }
    };
    let token = state.get_auth_token().await;
    let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };

    // Fetch skill candidates from daemon
    let candidates: Vec<serde_json::Value> = daemon_get(
        &state.http_client,
        &format!("/vaults/{}/activity-patterns/skill-candidates", urlencoding::encode(&vault_id)),
        tok,
    ).await.unwrap_or_default();

    if candidates.is_empty() { return Ok(vec![]); }

    // Take top 3 candidates to avoid LLM overload
    let mut suggestions = Vec::new();
    for candidate in candidates.into_iter().take(3) {
        let signature = candidate["signature"].as_str().unwrap_or("").to_string();
        let intent = candidate["semantic_intent"].as_str().unwrap_or("").to_string();
        let score = candidate["score"].as_f64().unwrap_or(0.0);
        if intent.is_empty() { continue; }

        let body = serde_json::json!({
            "messages": [
                {
                    "role": "system",
                    "content": "你是 AI 技能設計師。根據使用者的行為意圖，生成一個 agent skill 的設計。\n\
                                輸出 JSON，格式：{\"title\": \"技能標題\", \"trigger\": \"觸發關鍵字（逗號分隔，5-10個）\", \"behavior\": \"步驟式行為描述\"}\n\
                                只輸出 JSON，不加說明。"
                },
                {
                    "role": "user",
                    "content": format!("使用者行為意圖：{}", intent)
                }
            ],
            "stream": false,
            "temperature": 0.2,
            "max_tokens": 300,
        });

        let Ok(resp) = state.http_client
            .post(format!("{}/v1/chat/completions", base_url))
            .json(&body)
            .timeout(std::time::Duration::from_secs(20))
            .send().await
        else { continue; };

        let Ok(json) = resp.json::<serde_json::Value>().await else { continue; };
        let content = json["choices"][0]["message"]["content"].as_str().unwrap_or("");

        // Parse JSON output from LLM
        let parsed: serde_json::Value = if let Some(start) = content.find('{') {
            let snippet = &content[start..];
            let end = snippet.rfind('}').map(|i| i + 1).unwrap_or(snippet.len());
            serde_json::from_str(&snippet[..end]).unwrap_or_default()
        } else { continue; };

        let title = parsed["title"].as_str().unwrap_or("").to_string();
        let trigger = parsed["trigger"].as_str().unwrap_or("").to_string();
        let behavior = parsed["behavior"].as_str().unwrap_or("").to_string();
        if title.is_empty() || trigger.is_empty() { continue; }

        suggestions.push(SkillSuggestion {
            signature,
            score,
            intent,
            suggested_title: title,
            suggested_trigger: trigger,
            suggested_behavior: behavior,
        });
    }

    Ok(suggestions)
}

// ─── Progressive Distillation ──────────────────────────────────────────────────

/// 當某個 category 的 facts 超過 CONDENSE_THRESHOLD 時，呼叫 LLM 聚合成高階摘要，
/// 再呼叫 daemon /memory/distill 刪除原始 facts 並寫入摘要。
const CONDENSE_THRESHOLD: usize = 8;

#[tauri::command]
pub async fn condense_memory_facts(state: State<'_, AppState>) -> Result<u32, AppError> {
    let vault_id = match state.get_vault_id().await {
        Ok(v) if !v.is_empty() => v,
        _ => return Ok(0),
    };
    let base_url = {
        let port = *state.llama_actual_port.lock().await;
        match port {
            Some(p) => format!("http://127.0.0.1:{}", p),
            None => return Ok(0),
        }
    };
    let token = state.get_auth_token().await;
    let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };

    let categories = ["personal", "preference", "project", "rule", "general"];
    let mut condensed = 0u32;

    for cat in &categories {
        // Fetch all non-expired facts in this category
        let url = format!(
            "/vaults/{}/memory/query?category={}&limit=50",
            urlencoding::encode(&vault_id), cat
        );
        let facts: Vec<serde_json::Value> = daemon_get(&state.http_client, &url, tok)
            .await.unwrap_or_default();

        if facts.len() < CONDENSE_THRESHOLD { continue; }

        let source_ids: Vec<String> = facts.iter()
            .filter_map(|f| f["fact_id"].as_str().map(String::from))
            .collect();
        let facts_text = facts.iter()
            .filter_map(|f| f["content"].as_str())
            .enumerate()
            .map(|(i, c)| format!("{}. {}", i + 1, c))
            .collect::<Vec<_>>().join("\n");

        let body = serde_json::json!({
            "messages": [
                {
                    "role": "system",
                    "content": format!(
                        "你是記憶壓縮系統。以下是類別「{}」的記憶事實列表，請將它們整合成 2-4 條精煉的高階摘要，\
                         保留最重要的資訊，去除重複和過時內容。\
                         輸出格式：每條以「- 」開頭，不加編號，不加解釋。", cat
                    )
                },
                { "role": "user", "content": facts_text },
            ],
            "stream": false,
            "temperature": 0.1,
            "max_tokens": 300,
        });

        let Ok(resp) = state.http_client
            .post(format!("{}/v1/chat/completions", base_url))
            .json(&body)
            .timeout(std::time::Duration::from_secs(30))
            .send().await
        else { continue; };

        let Ok(json) = resp.json::<serde_json::Value>().await else { continue; };
        let summary = json["choices"][0]["message"]["content"].as_str().unwrap_or("");
        if summary.is_empty() { continue; }

        // POST distill to daemon
        if daemon_post::<_, serde_json::Value>(
            &state.http_client,
            &format!("/vaults/{}/memory/distill", urlencoding::encode(&vault_id)),
            &serde_json::json!({
                "category": cat,
                "source_ids": source_ids,
                "summary": summary,
            }),
            tok,
        ).await.is_ok() {
            condensed += 1;
        }
    }

    Ok(condensed)
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
