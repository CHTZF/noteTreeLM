use crate::db::SurrealDb;
use super::trace::SessionTrace;

/// Retention policy for session traces.
/// Keep traces no older than this many seconds (30 days).
const TRACE_RETENTION_SECS: i64 = 30 * 24 * 3600;
/// Hard cap: if the account has more than this many traces, delete the oldest.
const TRACE_MAX_PER_ACCOUNT: i64 = 100;

/// Persist a completed [`SessionTrace`] to the `session_traces` table,
/// then enforce the retention policy for this account.
///
/// Retention: keep at most `TRACE_MAX_PER_ACCOUNT` traces AND traces no older
/// than `TRACE_RETENTION_SECS`. The stricter of the two applies.
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
        return;
    }

    // ── Retention cleanup (best-effort, non-blocking errors ignored) ─────────
    let cutoff = chrono::Utc::now().timestamp() - TRACE_RETENTION_SECS;

    // Delete traces older than the retention window.
    let _ = db
        .query("DELETE session_traces WHERE account_id = $aid AND started_at < $cutoff")
        .bind(("aid",    account_id.to_string()))
        .bind(("cutoff", cutoff))
        .await;

    // Delete oldest traces beyond the per-account cap.
    // Use count-based approach: compute overflow then DELETE (SELECT id ... LIMIT overflow).
    // Avoids the SurrealQL NOT IN / LET variable matching pitfalls.
    #[derive(serde::Deserialize)]
    struct CountRow { count: i64 }
    let count: i64 = db
        .query("SELECT count() AS count FROM session_traces WHERE account_id = $aid GROUP ALL")
        .bind(("aid", account_id.to_string()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<CountRow>>(0).ok())
        .and_then(|rows| rows.into_iter().next().map(|r| r.count))
        .unwrap_or(0);

    let overflow = count - TRACE_MAX_PER_ACCOUNT;
    if overflow > 0 {
        let _ = db
            .query(
                "DELETE (SELECT id FROM session_traces \
                         WHERE account_id = $aid \
                         ORDER BY started_at ASC LIMIT $n)"
            )
            .bind(("aid", account_id.to_string()))
            .bind(("n",   overflow))
            .await;
    }
}
