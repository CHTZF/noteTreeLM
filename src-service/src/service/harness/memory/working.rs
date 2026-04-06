use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use serde_json::Value;
use crate::service::harness::governance::guard::{GuardOutcome, ToolCallRecord};

/// Per-session, in-memory store of tool execution evidence.
///
/// # Memory semantics
/// - **write** — [`record`]: called by Executor after every successful tool call.
///   Keyed by the LLM-assigned `tool_call_id`; multiple calls to the same id overwrite.
/// - **read** — [`with_records`]: synchronous snapshot scan over all accumulated evidence.
///   Used by guard evaluation (path-seen checks) and citation validation.
/// - **evict** — [`clear`]: wipe all records at end-of-session or on cancel.
///   The inner `Arc` is not dropped; only the `HashMap` contents are removed.
///   Other holders (e.g. `AgentSession.working_memory`) keep their reference to the same data.
///
/// `WorkingMemory` is `Clone` — cloning only bumps the refcount on the inner `Arc`;
/// all clones share the same backing store.
#[derive(Clone)]
pub(crate) struct WorkingMemory {
    inner: Arc<Mutex<HashMap<String, ToolCallRecord>>>,
}

impl WorkingMemory {
    /// Create a new, empty `WorkingMemory` for the session.
    pub(crate) fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Record a completed tool execution.
    ///
    /// `id`           — the LLM-assigned `tool_call_id` (used for citation lookup).
    /// `name`         — tool name (e.g. `"search_vault"`).
    /// `args`         — the arguments that were passed to the handler.
    /// `result`       — the value returned by the handler.
    /// `started_at`   — Unix timestamp (seconds) when execution started.
    /// `duration_ms`  — wall-clock duration of the tool call.
    /// `guard_outcome`— result of precondition guard evaluation.
    pub(crate) async fn record(
        &self,
        id:            impl Into<String>,
        name:          impl Into<String>,
        args:          Value,
        result:        Value,
        started_at:    i64,
        duration_ms:   u64,
        guard_outcome: GuardOutcome,
    ) {
        self.inner.lock().await.insert(
            id.into(),
            ToolCallRecord { name: name.into(), args, result, started_at, duration_ms, guard_outcome },
        );
    }

    /// Run a synchronous closure over the full evidence snapshot.
    ///
    /// Acquires the lock, calls `f` with a shared reference to the backing `HashMap`,
    /// drops the lock, and returns `f`'s return value. The closure must not be async —
    /// use this for guard evaluation and citation checks that need a single consistent view.
    ///
    /// The lock is held only for the duration of `f`, so callers must not call any other
    /// `WorkingMemory` method inside `f`.
    pub(crate) async fn with_records<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&HashMap<String, ToolCallRecord>) -> R,
    {
        f(&*self.inner.lock().await)
    }

    /// Evict all records. Called when a session ends or is cancelled.
    #[allow(dead_code)]
    pub(crate) async fn clear(&self) {
        self.inner.lock().await.clear();
    }

    /// Returns `true` if no tool calls have been recorded yet.
    #[allow(dead_code)]
    pub(crate) async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }

    /// Return (tool_name, args_summary) pairs for calls that appear ≥ 2 times
    /// (same tool name + identical `path` or `query` arg).
    /// Used by the round loop to inject a stall-detection warning.
    pub(crate) async fn repeated_calls(&self) -> Vec<(String, String)> {
        let map = self.inner.lock().await;
        let mut counts: std::collections::HashMap<(String, String), usize> =
            std::collections::HashMap::new();
        for rec in map.values() {
            let key_arg = rec.args["path"]
                .as_str()
                .or_else(|| rec.args["query"].as_str())
                .unwrap_or("")
                .to_string();
            *counts.entry((rec.name.clone(), key_arg)).or_insert(0) += 1;
        }
        counts.into_iter()
            .filter(|(_, n)| *n >= 2)
            .map(|((name, arg), _)| (name, arg))
            .collect()
    }

    /// Snapshot all records as a JSON-serialisable summary for `get_session_state`.
    /// Returns a Vec of `{name, args, guard_outcome, duration_ms}` objects (no result body).
    pub(crate) async fn snapshot_summary(&self) -> Vec<serde_json::Value> {
        use serde_json::json;
        let map = self.inner.lock().await;
        let mut records: Vec<_> = map.values().collect();
        // Sort by started_at for deterministic ordering.
        records.sort_by_key(|r| r.started_at);
        records.iter().map(|r| {
            let outcome = match &r.guard_outcome {
                GuardOutcome::Passed  => json!("passed"),
                GuardOutcome::Exempt  => json!("exempt"),
                GuardOutcome::Blocked(h) => json!({
                    "blocked": true,
                    "required_tool": h.required_tool,
                    "required_path": h.required_path,
                }),
            };
            json!({
                "name":         r.name,
                "args":         r.args,
                "guard":        outcome,
                "duration_ms":  r.duration_ms,
            })
        }).collect()
    }

}
