use serde_json::{json, Value};
use crate::db::SurrealDb;

/// Conversation-scoped turn log stored in SurrealDB `conversations.messages_json`.
///
/// # Memory semantics
/// - **read** — [`load`]: deserialise the stored message array for context-window assembly.
///   System messages are excluded; all other roles (user, assistant, tool) are returned as-is.
/// - **write** — [`append`]: replace the persisted message list with a caller-supplied slice.
///   Callers are responsible for filtering (e.g. drop tool_call/tool_result rows before saving).
/// - **evict** — [`trim`]: drop the oldest `(total − keep_last)` messages when the conversation
///   exceeds a retention budget, keeping the most recent `keep_last` turns.
///
/// All operations are idempotent with respect to the DB state: a missing record returns an
/// empty `Vec`; `append` always upserts; `trim` is a no-op when already within budget.
pub(crate) struct EpisodicMemory {
    db: SurrealDb,
}

#[allow(dead_code)]
impl EpisodicMemory {
    pub(crate) fn new(db: SurrealDb) -> Self {
        Self { db }
    }

    /// Load the persisted message history for `conv_id`.
    /// Returns an empty `Vec` when the conversation is new or `messages_json` is absent.
    /// Filters out system-role messages (those are rebuilt from the agent def each turn).
    pub(crate) async fn load(&self, conv_id: &str) -> Vec<Value> {
        #[derive(serde::Deserialize)]
        struct Row { messages_json: Option<String> }
        let rows: Vec<Row> = self.db
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

    /// Persist `messages` as the current conversation history for `conv_id`.
    ///
    /// Overwrites the previous blob; callers must supply the complete intended history
    /// (i.e. `load` first, then extend, then `append`).
    pub(crate) async fn append(&self, conv_id: &str, messages: &[Value]) {
        let now = chrono::Utc::now().timestamp();
        let json = serde_json::to_string(messages).unwrap_or_else(|_| "[]".to_string());
        let _ = self.db
            .query(
                "UPDATE conversations SET messages_json = $msgs, updated_at = $now \
                 WHERE record::id(id) = $cid",
            )
            .bind(("msgs", json))
            .bind(("now", now))
            .bind(("cid", conv_id.to_string()))
            .await;
    }

    /// Set a conversation title if it is still blank or the default placeholder.
    /// No-op if the title is already set.
    pub(crate) async fn set_title_if_blank(&self, conv_id: &str, messages: &[Value]) {
        #[derive(serde::Deserialize)]
        struct Row { title: String }
        let current: Option<String> = self.db
            .query("SELECT title FROM conversations WHERE record::id(id) = $cid LIMIT 1")
            .bind(("cid", conv_id.to_string()))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| r.title);

        match current.as_deref() {
            Some(t) if !t.is_empty() && t != "New Conversation" && t != "新對話" => return,
            _ => {}
        }

        let auto_title = messages.iter()
            .find(|m| m["role"].as_str() == Some("user"))
            .and_then(|m| m["content"].as_str())
            .map(|c| {
                let chars: String = c.chars().take(20).collect();
                if c.chars().count() > 20 { format!("{}…", chars) } else { chars }
            });

        if let Some(title) = auto_title {
            let now = chrono::Utc::now().timestamp();
            let _ = self.db
                .query(
                    "UPDATE conversations SET title = $title, updated_at = $now \
                     WHERE record::id(id) = $cid",
                )
                .bind(("title", title))
                .bind(("now", now))
                .bind(("cid", conv_id.to_string()))
                .await;
        }
    }

    /// Evict the oldest messages, retaining only the most recent `keep_last`.
    /// No-op when the stored count is already ≤ `keep_last`.
    pub(crate) async fn trim(&self, conv_id: &str, keep_last: usize) {
        let mut history = self.load(conv_id).await;
        if history.len() <= keep_last { return; }
        let drop_n = history.len() - keep_last;
        history.drain(..drop_n);
        self.append(conv_id, &history).await;
    }

    /// Return the conversation metadata (title, updated_at) for display, without loading
    /// the full message history.
    pub(crate) async fn meta(&self, conv_id: &str) -> Value {
        #[derive(serde::Deserialize)]
        struct Row { title: String, updated_at: i64 }
        self.db
            .query(
                "SELECT title, updated_at FROM conversations \
                 WHERE record::id(id) = $cid LIMIT 1",
            )
            .bind(("cid", conv_id.to_string()))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| json!({ "title": r.title, "updated_at": r.updated_at }))
            .unwrap_or(json!(null))
    }
}
