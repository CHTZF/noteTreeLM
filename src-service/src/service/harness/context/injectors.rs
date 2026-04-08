use crate::db::SurrealDb;
use super::super::prompt::{templates, Locale};

/// Load recent execution history for `conv_id` and format it as a system message string.
/// Returns `None` when no history exists.
pub(super) async fn load_exec_history(db: &SurrealDb, conv_id: &str) -> Option<String> {
    super::super::observability::trace_store::load_exec_history_for_conv(db, conv_id, 3).await
}

/// Load the task checkpoint for `conv_id` and format it as a system message string.
/// Returns `None` when no checkpoint exists.
pub(super) async fn load_checkpoint(db: &SurrealDb, conv_id: &str, locale: Locale) -> Option<String> {
    if conv_id.is_empty() { return None; }

    #[derive(serde::Deserialize)]
    struct Row { summary: String, remaining: String }

    let mut resp = db
        .query("SELECT summary, remaining FROM task_checkpoints WHERE conv_id = $cid LIMIT 1")
        .bind(("cid", conv_id.to_string()))
        .await
        .ok()?;

    let rows: Vec<Row> = resp.take(0).ok()?;
    let row = rows.into_iter().next()?;
    if row.summary.is_empty() && row.remaining.is_empty() { return None; }

    Some(templates::checkpoint_injection(&row.summary, &row.remaining, locale))
}

/// Load all agent knowledge entries for a vault and format them as a system message.
/// Returns `None` when no knowledge has been saved yet.
pub(super) async fn load_agent_knowledge(db: &SurrealDb, vault_id: &str, locale: Locale) -> Option<String> {
    if vault_id.is_empty() { return None; }

    #[derive(serde::Deserialize)]
    struct Row { key: String, content: String }

    let mut resp = db
        .query("SELECT key, content FROM agent_knowledge \
                WHERE vault_id = $vid ORDER BY updated_at ASC")
        .bind(("vid", vault_id.to_string()))
        .await
        .ok()?;

    let rows: Vec<Row> = resp.take(0).ok()?;
    if rows.is_empty() { return None; }

    let entries = rows.iter()
        .map(|r| format!("**{}**: {}", r.key, r.content))
        .collect::<Vec<_>>()
        .join("\n");

    Some(templates::agent_knowledge_injection(&entries, locale))
}
