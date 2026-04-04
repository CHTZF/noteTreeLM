use serde_json::{json, Value};
use crate::db::SurrealDb;

/// Load messages from DB by conversation_id (excludes system messages).
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
    let mut resp = match db
        .query("SELECT meta::id(id) as id, def_id, account_id, name, description, kind, tool_names, system_prompt, max_rounds, is_active, is_builtin, trigger, status, skill_ids FROM agent_definitions WHERE name = $name AND account_id = $aid LIMIT 1")
        .bind(("name", name.to_string()))
        .bind(("aid", account_id.to_string()))
        .await
    {
        Ok(r) => r,
        Err(e) => { tracing::error!("[load_agent_def] query error: {}", e); return None; }
    };
    let rows: Vec<Value> = match resp.take(0) {
        Ok(r) => r,
        Err(e) => { tracing::error!("[load_agent_def] take(0) error: {}", e); return None; }
    };
    let result = rows.into_iter().next();
    if result.is_none() {
        tracing::warn!("[load_agent_def] NOT FOUND: name={} account_id={}", name, account_id);
    }
    result
}

/// Query memory facts using semantic search (embedding cosine similarity).
/// Falls back to keyword regex if embedding server is unavailable or query embed fails.
/// Never errors; returns empty vec on failure.
pub(crate) async fn vault_query_memory_with_limit(
    client: &reqwest::Client,
    embedding_url: &Option<String>,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    keywords: &[String],
    limit: u64,
) -> Vec<Value> {
    let now = chrono::Utc::now().timestamp();
    #[derive(serde::Deserialize)]
    struct Row { fact_id: Option<String>, content: String, category: String, embedding: Option<Vec<f32>> }

    // Try semantic search first when we have keywords and an embedding server
    if !keywords.is_empty() {
        let query_text = keywords.join(" ");
        if let Some(query_vec) = crate::processing::embedder::embed_text(client, embedding_url, &query_text).await {
            if !query_vec.is_empty() {
                // Fetch all non-expired facts with their embeddings, score in-process
                let rows: Vec<Row> = db
                    .query("SELECT fact_id, content, category, embedding FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now AND embedding IS NOT NONE")
                    .bind(("vid", vault_id.to_string()))
                    .bind(("aid", account_id.to_string()))
                    .bind(("now", now))
                    .await
                    .ok()
                    .and_then(|mut r| r.take(0).ok())
                    .unwrap_or_default();

                if !rows.is_empty() {
                    let mut scored: Vec<(f32, Row)> = rows.into_iter().filter_map(|row| {
                        let emb = row.embedding.as_ref()?;
                        if emb.is_empty() { return None; }
                        let score = crate::processing::embedder::cosine_sim(&query_vec, emb);
                        Some((score, row))
                    }).collect();
                    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                    return scored.into_iter()
                        .take(limit as usize)
                        .map(|(_, r)| json!({
                            "fact_id": r.fact_id.unwrap_or_default(),
                            "content": r.content,
                            "category": r.category,
                        }))
                        .collect();
                }
            }
        }
    }

    // Fallback: no embedding server, no keywords, or no facts with embeddings — use recency / keyword regex
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

// ── Context pipeline stages (superseded by harness::context_pipeline) ────────
//
// These functions are kept as thin shims so that any external callers (scheduled
// agents, tests) continue to compile without changes. New code should use
// `harness::context_pipeline::ContextPipeline` directly.

#[allow(dead_code)]
pub(crate) async fn build_messages(
    db: &SurrealDb,
    conv_id: &str,
    input: &str,
    system: &str,
    activity_context: Option<&str>,
    system_injection: &str,
) -> Vec<Value> {
    use crate::service_agent::harness::context_pipeline::{ContextBudget, ContextInput, ContextPipeline};
    // Use a dummy client/url — history trim won't fire unless history is very long.
    let client = reqwest::Client::new();
    let pipeline = ContextPipeline::new(ContextBudget::default());
    let built = pipeline.build(
        ContextInput {
            db,
            conv_id,
            user_input: input,
            system_prompt: system,
            skill_injection: system_injection,
            activity_context,
            memory_facts: &[],
        },
        &client,
        "",   // llm_url — trim won't fire at empty url
    ).await;
    built.messages
}

#[allow(dead_code)]
pub(crate) fn inject_memory(msgs: &mut Vec<Value>, facts: &[Value]) {
    if facts.is_empty() { return; }
    let mem_block = format!("\n\n## 相關記憶\n{}",
        facts.iter().map(|f| format!("[{}] {}",
            f["category"].as_str().unwrap_or("general"),
            f["content"].as_str().unwrap_or("")
        )).collect::<Vec<_>>().join("\n")
    );
    if let Some(sys) = msgs.first_mut().filter(|m| m["role"].as_str() == Some("system")) {
        let old = sys["content"].as_str().unwrap_or("").to_string();
        sys["content"] = json!(format!("{}{}", old, mem_block));
    }
}

#[allow(dead_code)]
pub(crate) async fn trim_context(
    _msgs: &mut Vec<Value>,
    _client: &reqwest::Client,
    _llm_url: &str,
) {
    // Trimming is now handled inside ContextPipeline::build().
    // This shim is a no-op kept for compilation compatibility.
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
