use crate::db::SurrealDb;
use super::trace::SessionTrace;

/// Persist a completed [`SessionTrace`] to the `session_traces` table.
///
/// One record per agent turn. The `conv_id` field links the trace to its
/// episodic conversation history, enabling `read_session_with_conversation`
/// to load both together for trace_analyst analysis.
pub(crate) async fn save_session_trace(
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    trace: &SessionTrace,
) {
    let payload = match serde_json::to_value(trace) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("vault_id".to_string(), serde_json::json!(vault_id));
                obj.insert("account_id".to_string(), serde_json::json!(account_id));
            }
            v
        }
        Err(e) => {
            tracing::warn!("[trace_store] serialise error: {}", e);
            return;
        }
    };

    if let Err(e) = db
        .query("CREATE session_traces CONTENT $data")
        .bind(("data", payload))
        .await
    {
        tracing::warn!("[trace_store] write error: {}", e);
    }
}
