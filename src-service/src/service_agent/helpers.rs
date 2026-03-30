use std::sync::Arc;
use serde_json::{json, Value};
use crate::db::SurrealDb;

/// Call llama-server /embedding endpoint (same format as llama.cpp legacy endpoint).
pub(crate) async fn embed_text_llm(client: &reqwest::Client, llm_url: &str, text: &str) -> Vec<f32> {
    fn extract_vec(json: &Value) -> Vec<f32> {
        if let Some(arr) = json["data"][0]["embedding"].as_array() {
            let v: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
            if !v.is_empty() { return v; }
        }
        if let Some(arr) = json["embedding"].as_array() {
            let v: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
            if !v.is_empty() { return v; }
        }
        if let Some(first) = json.as_array().and_then(|a| a.first()) {
            if let Some(arr) = first["embedding"].as_array() {
                let v: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
                if !v.is_empty() { return v; }
            }
        }
        vec![]
    }
    // Try OpenAI /v1/embeddings format first
    if let Ok(resp) = client
        .post(format!("{}/v1/embeddings", llm_url))
        .json(&json!({"model": "text-embedding", "input": text}))
        .send().await
    {
        if resp.status().is_success() {
            if let Ok(j) = resp.json::<Value>().await {
                let v = extract_vec(&j);
                if !v.is_empty() { return v; }
            }
        }
    }
    // Fallback: llama.cpp legacy /embedding
    if let Ok(resp) = client
        .post(format!("{}/embedding", llm_url))
        .json(&json!({"content": text}))
        .send().await
    {
        if resp.status().is_success() {
            if let Ok(j) = resp.json::<Value>().await {
                let v = extract_vec(&j);
                if !v.is_empty() { return v; }
            }
        }
    }
    vec![]
}

/// Cosine similarity (dot product of L2-normalized vectors).
pub(crate) fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() { return 0.0; }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { return 0.0; }
    dot / (na * nb)
}



/// Load messages from DB by conversation_id.
/// Returns vec of {role, content} objects (no system messages).
pub(crate) async fn load_messages_db(db: &SurrealDb, conv_id: &str) -> Vec<Value> {
    #[derive(serde::Deserialize)]
    struct Row { messages_json: Option<String> }
    let rows: Vec<Row> = db
        .query("SELECT messages_json FROM conversations WHERE record::id(id) = $cid LIMIT 1")
        .bind(("cid", conv_id.to_string()))
        .await
        .ok()
        .and_then(|mut r| r.take(0).ok())
        .unwrap_or_default();
    let json_str = rows.into_iter().next()
        .and_then(|r| r.messages_json)
        .unwrap_or_else(|| "[]".to_string());
    serde_json::from_str::<Vec<Value>>(&json_str)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m["role"].as_str() != Some("system"))
        .collect()
}

/// Load agent definition from DB by name + account_id.
pub(crate) async fn load_agent_def(
    db: &SurrealDb,
    name: &str,
    account_id: &str,
) -> Option<Value> {
    let mut resp = db
        .query("SELECT * FROM agent_definitions WHERE name = $name AND account_id = $aid LIMIT 1")
        .bind(("name", name.to_string()))
        .bind(("aid", account_id.to_string()))
        .await
        .ok()?;

    let rows: Vec<Value> = resp.take(0).ok()?;
    rows.into_iter().next()
}

/// Search agent skills from SurrealDB and filter by input.
/// Returns Vec<(skill_id, title, behavior, tool_calls, need_tool_chain, tool_chain_order, injection_mode)>
pub(crate) async fn search_skills_db(
    db: &SurrealDb,
    account_id: &str,
    input: &str,
) -> Vec<(String, String, String, Vec<String>, bool, Vec<String>, String)> {
    let mut resp = match db
        .query("SELECT *, record::id(id) AS id FROM agent_skills WHERE account_id = $aid AND is_active = true")
        .bind(("aid", account_id.to_string()))
        .await
    {
        Ok(r) => r,
        Err(_) => return vec![],
    };
    let skills: Vec<Value> = resp.take(0).unwrap_or_default();
    let use_ask_lower = input.to_lowercase();
    skills.iter().filter(|s| {
        let trigger = s["trigger"].as_str().unwrap_or("").to_lowercase();
        let mode = s["injection_mode"].as_str().unwrap_or("passive");
        if mode == "active" || mode == "proactive" { return true; }
        trigger.split(['、', ',', '，']).any(|kw| {
            let kw = kw.trim();
            !kw.is_empty() && use_ask_lower.contains(kw)
        })
    }).map(|s| {
        let skill_id = s["skill_id"].as_str().unwrap_or("").to_string();
        let title = s["title"].as_str().unwrap_or("").to_string();
        let behavior = s["behavior"].as_str().unwrap_or("").to_string();
        let tool_calls: Vec<String> = if let Some(arr) = s["tool_calls"].as_array() {
            arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
        } else if let Some(s) = s["tool_calls"].as_str() {
            serde_json::from_str::<Vec<String>>(s).unwrap_or_default()
        } else { vec![] };
        let need_tool_chain = s["need_tool_chain"].as_bool().unwrap_or(false);
        let tool_chain_order: Vec<String> = if let Some(arr) = s["tool_chain_order"].as_array() {
            arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
        } else { vec![] };
        let injection_mode = s["injection_mode"].as_str().unwrap_or("passive").to_string();
        (skill_id, title, behavior, tool_calls, need_tool_chain, tool_chain_order, injection_mode)
    }).collect()
}

/// Detect whether a response contains a reusable structured framework.
pub(crate) fn detect_response_framework(text: &str) -> bool {
    let has_numbered = (text.contains("1.") || text.contains("1、") || text.contains("①"))
        && (text.contains("2.") || text.contains("2、") || text.contains("②"));
    let has_sequential = (text.contains("先") && text.contains("再") && text.contains("最後"))
        || (text.contains("首先") && text.contains("接著"));
    let has_framework_kw = text.contains("步驟") || text.contains("流程") || text.contains("規範");
    text.len() > 300 && (has_numbered || has_sequential || has_framework_kw)
}

/// Convenience wrapper: query memory facts and return Vec<Value> (never errors, returns empty vec).
pub(crate) async fn vault_query_memory_with_limit(
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    keywords: &[String],
    limit: u64,
) -> Vec<Value> {
    let now = chrono::Utc::now().timestamp();
    #[derive(serde::Deserialize)]
    struct Row { fact_id: Option<String>, content: String, category: String }
    let rows: Vec<Row> = if keywords.is_empty() {
        db.query("SELECT fact_id, content, category FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now ORDER BY created_at DESC LIMIT $lim")
            .bind(("vid", vault_id.to_string()))
            .bind(("aid", account_id.to_string()))
            .bind(("now", now))
            .bind(("lim", limit))
            .await
            .ok()
            .and_then(|mut r| r.take(0).ok())
            .unwrap_or_default()
    } else {
        let mut collected: Vec<Row> = Vec::new();
        for kw in keywords.iter().take(3) {
            let mut rows: Vec<Row> = db
                .query("SELECT fact_id, content, category FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND content ~ $kw AND expires_at > $now LIMIT $lim")
                .bind(("vid", vault_id.to_string()))
                .bind(("aid", account_id.to_string()))
                .bind(("kw", kw.clone()))
                .bind(("now", now))
                .bind(("lim", limit))
                .await
                .ok()
                .and_then(|mut r| r.take(0).ok())
                .unwrap_or_default();
            collected.append(&mut rows);
        }
        collected
    };
    rows.into_iter().map(|r| json!({
        "fact_id": r.fact_id.unwrap_or_default(),
        "content": r.content,
        "category": r.category,
    })).collect()
}
