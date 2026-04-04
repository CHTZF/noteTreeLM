use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use serde_json::Value;
use crate::state::ToolCallRecord;

/// Per-session, in-memory store of tool execution evidence.
///
/// # Memory semantics
/// - **write** — [`record`]: called by Executor after every successful tool call.
///   Keyed by the LLM-assigned `tool_call_id`; multiple calls to the same id overwrite.
/// - **read** — [`with_records`]: synchronous snapshot scan over all accumulated evidence.
///   Used by guard evaluation (path-seen checks) and citation validation.
/// - **evict** — [`clear`]: wipe all records at end-of-session or on cancel.
///   The inner `Arc` is not dropped; only the `HashMap` contents are removed.
///   Other holders (e.g. `AgentSession.tool_calls`) keep their reference to the same data.
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
        guard_outcome: crate::state::GuardOutcome,
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

    /// Expose the raw `Arc<Mutex<HashMap<...>>>` for callers that still hold the legacy
    /// `Arc<Mutex<HashMap>>` type (e.g. `AgentSession.tool_calls` in `state.rs`).
    ///
    /// Use typed methods above for all write and query operations; this accessor is
    /// a bridge shim only — do not introduce new direct-lock callers.
    pub(crate) fn as_raw(&self) -> &Arc<Mutex<HashMap<String, ToolCallRecord>>> {
        &self.inner
    }
}
