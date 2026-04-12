use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex;
use serde_json::Value;
use crate::service::harness::governance::guard::{GuardOutcome, ToolCallRecord};

/// Recursively collect all `__cite_id__` string values from a JSON value.
/// Used to discover nested cite IDs (e.g. `kb_1`, `kb_2`) embedded inside tool results.
fn collect_cite_ids_from_value(v: &Value, out: &mut std::collections::HashSet<String>) {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("__cite_id__") {
                out.insert(id.clone());
            }
            for child in map.values() {
                collect_cite_ids_from_value(child, out);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_cite_ids_from_value(item, out);
            }
        }
        _ => {}
    }
}

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

/// Max number of `recall` calls allowed per session to prevent recall-loop exhaustion.
const MAX_RECALL_CALLS: usize = 5;

#[derive(Clone)]
pub(crate) struct WorkingMemory {
    /// Persistent evidence store — carries over across messages in the same conversation.
    /// Keyed by tool_call_id; used for cite validation, guard evaluation, and recall.
    inner:           Arc<Mutex<HashMap<String, ToolCallRecord>>>,
    /// IDs recorded in the CURRENT run only (cleared by `start_new_run`).
    /// Used to restrict stall detection to within a single user message so that
    /// calling `list_emails` in message 1 does not block it in message 2.
    current_run_ids: Arc<Mutex<std::collections::HashSet<String>>>,
    recall_count:    Arc<AtomicUsize>,
}

impl WorkingMemory {
    /// Create a new, empty `WorkingMemory` for the session.
    pub(crate) fn new() -> Self {
        Self {
            inner:           Arc::new(Mutex::new(HashMap::new())),
            current_run_ids: Arc::new(Mutex::new(std::collections::HashSet::new())),
            recall_count:    Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Called at the start of each new user-message run.
    /// Resets per-run state (stall detection, recall cap) while preserving the
    /// persistent evidence store (`inner`) so previous tool results remain citable.
    pub(crate) async fn start_new_run(&self) {
        self.current_run_ids.lock().await.clear();
        self.recall_count.store(0, Ordering::Relaxed);
    }

    /// Returns true if at least one tool call was recorded in the current run.
    /// Used by cite enforcement: only require cite tags when tools have actually
    /// returned results — if no tools ran (e.g. all failed), the LLM is free to
    /// respond without a cite tag.
    pub(crate) async fn current_run_has_results(&self) -> bool {
        !self.current_run_ids.lock().await.is_empty()
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
        let id = id.into();
        self.current_run_ids.lock().await.insert(id.clone());
        self.inner.lock().await.insert(
            id,
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
        let run_ids = self.current_run_ids.lock().await;
        let map = self.inner.lock().await;
        let mut counts: std::collections::HashMap<(String, String), usize> =
            std::collections::HashMap::new();
        // Only count calls from the current run to avoid cross-message false stalls.
        for (id, rec) in map.iter() {
            if !run_ids.contains(id) { continue; }
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

    /// Keyword search over recorded tool calls.
    ///
    /// Returns compact summaries matching `query` (case-insensitive substring match on
    /// tool_name, args JSON, and the first 500 chars of result JSON).
    ///
    /// Returns `Err` if the per-session recall cap (`MAX_RECALL_CALLS`) is exceeded,
    /// preventing context-exhaustion loops where recall results trigger more recalls.
    /// `recall` calls are never recorded in working_memory themselves, so they cannot
    /// appear in their own results.
    pub(crate) async fn recall(&self, query: &str) -> Result<Vec<Value>, String> {
        let n = self.recall_count.fetch_add(1, Ordering::Relaxed) + 1;
        if n > MAX_RECALL_CALLS {
            return Err(format!(
                "recall 已達本 session 上限（{}次），請直接根據現有資訊回答。",
                MAX_RECALL_CALLS
            ));
        }
        let q = query.to_lowercase();
        let map = self.inner.lock().await;
        let mut records: Vec<_> = map.values().collect();
        records.sort_by_key(|r| r.started_at);
        let results: Vec<Value> = records.iter().filter_map(|r| {
            let args_str  = r.args.to_string().to_lowercase();
            let result_preview = r.result.to_string();
            let result_str = result_preview[..result_preview.floor_char_boundary(500)].to_lowercase();
            let name_str  = r.name.to_lowercase();
            if name_str.contains(&q) || args_str.contains(&q) || result_str.contains(&q) {
                Some(serde_json::json!({
                    "tool":     r.name,
                    "args":     r.args,
                    "result":   &result_preview[..result_preview.floor_char_boundary(800)],
                }))
            } else {
                None
            }
        }).take(6).collect();
        Ok(results)
    }

    /// Collect all cite IDs that are valid for this session.
    ///
    /// Includes:
    /// - Every tool_call_id key (outer `__cite_id__` injected by `annotate_cite_id`)
    /// - Any `__cite_id__` string values found recursively inside tool results
    ///   (e.g. `kb_1`, `kb_2` embedded by `search_kb_pages`)
    ///
    /// Used by citation validation so IDs from both levels are accepted.
    pub(crate) async fn all_valid_cite_ids(&self) -> std::collections::HashSet<String> {
        let map = self.inner.lock().await;
        let mut ids = std::collections::HashSet::new();
        for (key, rec) in map.iter() {
            ids.insert(key.clone());
            collect_cite_ids_from_value(&rec.result, &mut ids);
        }
        ids
    }

    /// Look up a single cite ID and return `(tool_name, result_preview)` if found.
    /// `result_preview` is the first 300 chars of the result JSON string.
    /// Returns `None` if the ID is not a top-level tool_call_id key.
    pub(crate) async fn cite_detail(&self, id: &str) -> Option<(String, String)> {
        let map = self.inner.lock().await;
        map.get(id).map(|rec| {
            let preview = rec.result.to_string();
            let preview = preview[..preview.floor_char_boundary(300)].to_string();
            (rec.name.clone(), preview)
        })
    }

    /// Return top-level tool call records sorted by `started_at`, as `(id, tool_name)` pairs.
    /// Only includes the direct `tool_call_id` keys (not nested `__cite_id__` values inside
    /// results), so the LLM can see exactly which tool produced each citable result.
    pub(crate) async fn cite_id_tool_pairs(&self) -> Vec<(String, String)> {
        let map = self.inner.lock().await;
        let mut pairs: Vec<_> = map.iter()
            .map(|(id, rec)| (rec.started_at, id.clone(), rec.name.clone()))
            .collect();
        pairs.sort_by_key(|(ts, _, _)| *ts);
        pairs.into_iter().map(|(_, id, name)| (id, name)).collect()
    }

    /// Same as `cite_id_tool_pairs` but restricted to the CURRENT RUN only.
    /// Used by `cite_reminder` so the LLM is only prompted to cite tools it called
    /// in this round, not carry-over results from previous messages.
    pub(crate) async fn current_run_cite_pairs(&self) -> Vec<(String, String)> {
        let run_ids = self.current_run_ids.lock().await;
        let map     = self.inner.lock().await;
        let mut pairs: Vec<_> = map.iter()
            .filter(|(id, _)| run_ids.contains(*id))
            .map(|(id, rec)| (rec.started_at, id.clone(), rec.name.clone()))
            .collect();
        pairs.sort_by_key(|(ts, _, _)| *ts);
        pairs.into_iter().map(|(_, id, name)| (id, name)).collect()
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
