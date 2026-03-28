/// memory_pipeline.rs
///
/// Pure-service background memory pipeline.
/// Triggered from conversations::save_messages when:
///   - user has enable_auto_memory = "true" in user_settings
///   - message count hits a multiple of memory_threshold
///
/// Steps:
///   1. extract_facts   — LLM萃取 3-5 條事實，存入 memory_facts
///   2. condense        — 若某 category ≥ CONDENSE_THRESHOLD，LLM壓縮後 distill
///   3. distill_prefs   — personal/preference/rule facts → upsert agent_skill
///   4. emit            — SSE memory:processed { facts_added }

use serde_json::{json, Value};
use crate::api_state::ApiState;
use crate::db::SurrealDb;

const CONDENSE_THRESHOLD: usize = 8;

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(
    state: ApiState,
    account_id: String,
    vault_id: String,
    conv_id: String,
    messages_json: String,
) {
    let llm_url = state.daemon.llm_url.read().await.clone();
    let Some(llm_url) = llm_url else { return };

    let emb_url = state.daemon.embedding_url.read().await.clone();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_default();

    let facts_added = extract_and_store_facts(
        &client, &state.db, llm_url.clone(), emb_url.clone(),
        account_id.clone(), vault_id.clone(), conv_id.clone(), messages_json,
    ).await;

    condense_if_needed(&client, &state.db, llm_url.clone(), vault_id.clone()).await;
    distill_preferences(&client, &state.db, llm_url, emb_url, account_id, vault_id.clone()).await;

    state.daemon.emit("memory:processed", json!({
        "conv_id": conv_id,
        "facts_added": facts_added,
    }));
}

// ── Step 1: Extract facts ─────────────────────────────────────────────────────

async fn extract_and_store_facts(
    client: &reqwest::Client,
    db: &SurrealDb,
    llm_url: String,
    emb_url: Option<String>,
    account_id: String,
    vault_id: String,
    conv_id: String,
    messages_json: String,
) -> u32 {
    // Parse messages
    let Ok(msgs) = serde_json::from_str::<Vec<Value>>(&messages_json) else { return 0 };
    let conv_text: String = msgs.iter()
        .filter(|m| matches!(m["role"].as_str(), Some("user") | Some("assistant")))
        .map(|m| {
            let role = m["role"].as_str().unwrap_or("user");
            let content: String = m["content"].as_str().unwrap_or("").chars().take(500).collect();
            format!("[{}]: {}", role, content)
        })
        .collect::<Vec<_>>()
        .join("\n");

    if conv_text.is_empty() { return 0; }

    // Fetch existing facts for dedup context
    let existing_summary = fetch_existing_facts_summary(db, vault_id.clone()).await;
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

    let body = json!({
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user",   "content": conv_text },
        ],
        "stream": false,
        "temperature": 0.1,
        "max_tokens": 400,
    });

    let Ok(resp) = client.post(format!("{}/v1/chat/completions", llm_url))
        .json(&body).send().await
    else { return 0 };

    let Ok(json) = resp.json::<Value>().await else { return 0 };
    let facts_text = json["choices"][0]["message"]["content"]
        .as_str().unwrap_or("").to_string();

    // Parse `[category] content` lines
    let valid_cats = ["personal", "preference", "project", "rule", "general"];
    let parsed: Vec<(String, String)> = facts_text.lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches("- ").trim_start_matches("* ");
            if !line.starts_with('[') { return None; }
            let close = line.find(']')?;
            let cat = line[1..close].trim().to_lowercase();
            let cat = if valid_cats.contains(&cat.as_str()) { cat } else { "general".to_string() };
            let content = line[close + 1..].trim().to_string();
            if content.is_empty() { return None; }
            Some((cat, content))
        })
        .collect();

    let count = parsed.len() as u32;
    if count == 0 { return 0 }

    let now = chrono::Utc::now().timestamp();
    let expires_at = now + 60 * 60 * 24 * 365; // 1 year

    for (cat, content) in parsed {
        let fact_id = uuid::Uuid::new_v4().to_string();
        let embedding = get_embedding(client, &emb_url, &content).await;
        let emb_str: Option<String> = if embedding.is_empty() { None }
            else { serde_json::to_string(&embedding).ok() };

        let _ = db
            .query("INSERT INTO memory_facts (fact_id, vault_id, account_id, conv_id, content, category, expires_at, created_at, embedding) VALUES ($fid, $vid, $aid, $cid, $content, $cat, $exp, $now, $emb)")
            .bind(("fid", fact_id))
            .bind(("vid", vault_id.clone()))
            .bind(("aid", account_id.clone()))
            .bind(("cid", conv_id.clone()))
            .bind(("content", content))
            .bind(("cat", cat))
            .bind(("exp", expires_at))
            .bind(("now", now))
            .bind(("emb", emb_str))
            .await;
    }

    count
}

// ── Step 2: Condense ──────────────────────────────────────────────────────────

async fn condense_if_needed(
    client: &reqwest::Client,
    db: &SurrealDb,
    llm_url: String,
    vault_id: String,
) {
    let categories = ["personal", "preference", "project", "rule", "general"];
    let now = chrono::Utc::now().timestamp();

    for cat in &categories {
        #[derive(serde::Deserialize)]
        struct FactRow { fact_id: String, content: String }

        let Ok(mut resp) = db
            .query("SELECT fact_id, content FROM memory_facts WHERE vault_id = $vid AND category = $cat AND expires_at > $now ORDER BY created_at DESC LIMIT 50")
            .bind(("vid", vault_id.clone()))
            .bind(("cat", cat.to_string()))
            .bind(("now", now))
            .await
        else { continue };

        let facts: Vec<FactRow> = resp.take(0).unwrap_or_default();
        if facts.len() < CONDENSE_THRESHOLD { continue }

        let source_ids: Vec<String> = facts.iter().map(|f| f.fact_id.clone()).collect();
        let facts_text = facts.iter().enumerate()
            .map(|(i, f)| format!("{}. {}", i + 1, f.content))
            .collect::<Vec<_>>().join("\n");

        let body = json!({
            "messages": [
                {
                    "role": "system",
                    "content": format!(
                        "你是記憶壓縮系統。以下是類別「{}」的記憶事實，請整合成 2-4 條精煉摘要，\
                         保留最重要資訊，去除重複和過時內容。\
                         每條以「- 」開頭，不加編號，不加解釋。", cat
                    )
                },
                { "role": "user", "content": facts_text },
            ],
            "stream": false,
            "temperature": 0.1,
            "max_tokens": 300,
        });

        let Ok(resp) = client.post(format!("{}/v1/chat/completions", llm_url))
            .json(&body).send().await
        else { continue };

        let Ok(json) = resp.json::<Value>().await else { continue };
        let summary = match json["choices"][0]["message"]["content"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };

        // Delete source facts
        for fid in &source_ids {
            let _ = db
                .query("DELETE FROM memory_facts WHERE fact_id = $fid")
                .bind(("fid", fid.clone()))
                .await;
        }

        // Insert condensed summary
        let new_fact_id = uuid::Uuid::new_v4().to_string();
        let expires_at = now + 60 * 60 * 24 * 365;
        let _ = db
            .query("INSERT INTO memory_facts (fact_id, vault_id, content, category, expires_at, created_at) VALUES ($fid, $vid, $content, $cat, $exp, $now)")
            .bind(("fid", new_fact_id))
            .bind(("vid", vault_id.clone()))
            .bind(("content", summary))
            .bind(("cat", cat.to_string()))
            .bind(("exp", expires_at))
            .bind(("now", now))
            .await;
    }
}

// ── Step 3: Distill preferences → agent_skill ────────────────────────────────

async fn distill_preferences(
    client: &reqwest::Client,
    db: &SurrealDb,
    llm_url: String,
    emb_url: Option<String>,
    account_id: String,
    vault_id: String,
) {
    let now = chrono::Utc::now().timestamp();

    #[derive(serde::Deserialize)]
    struct FactRow { content: String, category: String }

    let Ok(mut resp) = db
        .query("SELECT content, category FROM memory_facts WHERE vault_id = $vid AND category IN ['personal','preference','rule'] AND expires_at > $now ORDER BY created_at DESC LIMIT 30")
        .bind(("vid", vault_id.clone()))
        .bind(("now", now))
        .await
    else { return };

    let facts: Vec<FactRow> = resp.take(0).unwrap_or_default();
    if facts.is_empty() { return }

    let combined = facts.iter()
        .map(|f| format!("[{}] {}", f.category, f.content))
        .collect::<Vec<_>>().join("\n");

    let body = json!({
        "messages": [
            {
                "role": "system",
                "content": "你是使用者偏好分析系統。從以下記憶事實中，整理出使用者的偏好規則。\n\
                            輸出格式：條列式，每條以「- 」開頭，簡潔描述。不超過 15 條。只輸出列表，不要說明。"
            },
            { "role": "user", "content": format!("記憶事實：\n{}", combined) }
        ],
        "max_tokens": 512,
        "temperature": 0.3,
        "stream": false,
    });

    let Ok(resp) = client.post(format!("{}/v1/chat/completions", llm_url))
        .json(&body).send().await
    else { return };

    let Ok(json) = resp.json::<Value>().await else { return };
    let prefs = match json["choices"][0]["message"]["content"].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return,
    };

    let updated_time = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    let behavior = format!("更新時間：{}\n\n{}", updated_time, prefs);

    let emb = get_embedding(client, &emb_url, &behavior).await;
    let emb_json: Option<String> = if emb.is_empty() { None } else { serde_json::to_string(&emb).ok() };

    let skill_id = "distilled_user_prefs".to_string();

    // Check if skill exists for this account
    let existing: Vec<Value> = db
        .query("SELECT record::id(id) AS id FROM agent_skills WHERE account_id = $aid AND skill_id = $sid LIMIT 1")
        .bind(("aid", account_id.clone()))
        .bind(("sid", skill_id.clone()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<Value>>(0).ok())
        .unwrap_or_default();

    if existing.is_empty() {
        let _ = db
            .query("INSERT INTO agent_skills (account_id, skill_id, title, trigger, behavior, is_active, injection_mode, embedding, created_at, updated_at) VALUES ($aid, $sid, $title, $trigger, $behavior, true, 'system', $emb, $now, $now)")
            .bind(("aid", account_id.clone()))
            .bind(("sid", skill_id.clone()))
            .bind(("title", "使用者偏好（自動蒸餾）".to_string()))
            .bind(("trigger", "__auto_injected__".to_string()))
            .bind(("behavior", behavior.clone()))
            .bind(("emb", emb_json.clone()))
            .bind(("now", now))
            .await;
    } else {
        let _ = db
            .query("UPDATE agent_skills SET behavior = $behavior, embedding = $emb, updated_at = $now WHERE account_id = $aid AND skill_id = $sid")
            .bind(("behavior", behavior.clone()))
            .bind(("emb", emb_json))
            .bind(("now", now))
            .bind(("aid", account_id))
            .bind(("sid", skill_id.clone()))
            .await;
    }

    // Also update vault-scoped copy for backward compat
    let _ = db
        .query("UPDATE agent_skills SET behavior = $behavior, updated_at = $now WHERE vault_id = $vid AND skill_id = $sid")
        .bind(("behavior", behavior))
        .bind(("now", now))
        .bind(("vid", vault_id))
        .bind(("sid", skill_id))
        .await;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn fetch_existing_facts_summary(db: &SurrealDb, vault_id: String) -> String {
    let now = chrono::Utc::now().timestamp();

    #[derive(serde::Deserialize)]
    struct Row { content: String, category: String }

    let Ok(mut resp) = db
        .query("SELECT content, category FROM memory_facts WHERE vault_id = $vid AND expires_at > $now ORDER BY created_at DESC LIMIT 20")
        .bind(("vid", vault_id))
        .bind(("now", now))
        .await
    else { return String::new() };

    let rows: Vec<Row> = resp.take(0).unwrap_or_default();
    rows.iter()
        .map(|r| format!("[{}] {}", r.category, r.content))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn get_embedding(
    client: &reqwest::Client,
    emb_url: &Option<String>,
    text: &str,
) -> Vec<f32> {
    let Some(url) = emb_url else { return vec![] };
    let Ok(resp) = client
        .post(format!("{}/embedding", url))
        .json(&json!({ "input": text }))
        .timeout(std::time::Duration::from_secs(10))
        .send().await
    else { return vec![] };
    let Ok(json) = resp.json::<Value>().await else { return vec![] };
    json["embedding"].as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect())
        .unwrap_or_default()
}

// ── User settings helpers ─────────────────────────────────────────────────────

pub async fn get_user_memory_settings(db: &SurrealDb, account_id: &str) -> (bool, u32) {
    #[derive(serde::Deserialize)]
    struct Row { key: String, value: String }

    let Ok(mut resp) = db
        .query("SELECT `key`, `value` FROM user_settings WHERE username = $u AND `key` IN ['enable_auto_memory', 'memory_threshold']")
        .bind(("u", account_id.to_string()))
        .await
    else { return (false, 20) };

    let rows: Vec<Row> = resp.take(0).unwrap_or_default();
    let mut enabled = false;
    let mut threshold = 20u32;
    for r in rows {
        match r.key.as_str() {
            "enable_auto_memory" => enabled = r.value == "true",
            "memory_threshold"   => threshold = r.value.parse().unwrap_or(20),
            _ => {}
        }
    }
    (enabled, threshold)
}
