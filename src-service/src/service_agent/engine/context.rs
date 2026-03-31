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
    let mut resp = db
        .query("SELECT * FROM agent_definitions WHERE name = $name AND account_id = $aid LIMIT 1")
        .bind(("name", name.to_string()))
        .bind(("aid", account_id.to_string()))
        .await
        .ok()?;
    let rows: Vec<Value> = resp.take(0).ok()?;
    rows.into_iter().next()
}

/// Query memory facts with optional keyword filter. Never errors; returns empty vec on failure.
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

/// Detect whether a response contains a reusable structured framework.
pub(crate) fn detect_response_framework(text: &str) -> bool {
    let has_numbered = (text.contains("1.") || text.contains("1、") || text.contains("①"))
        && (text.contains("2.") || text.contains("2、") || text.contains("②"));
    let has_sequential = (text.contains("先") && text.contains("再") && text.contains("最後"))
        || (text.contains("首先") && text.contains("接著"));
    let has_framework_kw = text.contains("步驟") || text.contains("流程") || text.contains("規範");
    text.len() > 300 && (has_numbered || has_sequential || has_framework_kw)
}
