use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::db::SurrealDb;
use crate::service_agent::engine::executor::Executor;
use crate::service_agent::engine::planner::Planner;
use crate::service_agent::engine::tool_registry::ToolRegistry;
use crate::service_agent::engine::transaction::Transaction;
use crate::service_agent::harness::governance::guard::evaluate_guard;
use crate::service_agent::harness::memory::working::WorkingMemory;
use crate::service_agent::harness::observability::trace::SessionTrace;
use crate::service_agent::harness::tool_def::find_tool_def;
use crate::service_agent::types::{EmitEventFn, IsWriteFn, Tool};

use super::{EvalCase, EvalResult, MockToolCall};

/// Runs [`EvalCase`]s against the real Executor + guard + WorkingMemory pipeline
/// without any LLM, DB, or filesystem I/O.
///
/// # How it works
/// 1. Build a mock `ToolRegistry`: each tool applies the **real guard** from
///    `ALL_TOOL_DEFS`, then returns the predefined `mock_result` on success.
/// 2. Execute the tool sequence through the real `Executor` (Planner linear graph).
/// 3. Assemble a `SessionTrace` from `WorkingMemory` (sorted for deterministic ordering).
/// 4. Evaluate all `TraceAssertion`s and return `EvalResult`.
pub(crate) struct EvalRunner;

impl EvalRunner {
    pub(crate) async fn run(case: &EvalCase) -> EvalResult {
        let working_memory = WorkingMemory::new();
        let registry = build_eval_registry(&case.tool_sequence, working_memory.clone());

        // Assign deterministic eval ids so WorkingMemory keys are sortable.
        let calls: Vec<(String, String, Value)> = case.tool_sequence.iter()
            .enumerate()
            .map(|(i, tc)| {
                let id = if tc.id.is_empty() {
                    format!("eval_{:02}", i)
                } else {
                    tc.id.clone()
                };
                (id, tc.name.clone(), tc.args.clone())
            })
            .collect();

        let graph = Planner::plan(&calls);

        // Silent emit — no SSE in eval runs.
        let emit_fn: EmitEventFn = Arc::new(|_, _| {});
        // Treat all tools as read-only: skip write-confirmation flow entirely.
        // The guard is the protection mechanism being tested; the confirmation
        // UX is an orthogonal concern that requires a live frontend to resolve.
        let is_write_fn: IsWriteFn = Arc::new(|_| false);
        let tx = Arc::new(Transaction::new());

        let executor = Executor::new(
            Arc::new(registry),
            emit_fn,
            is_write_fn,
            working_memory.clone(),
        );

        // Execute. Guard-blocked tools return Ok(sentinel), not Err — so we never
        // hit the Err branch. Any Err from a mock is a bug in the test setup.
        let _ = executor.execute_graph(graph, tx).await;

        // Collect tool call records sorted by their WorkingMemory key (eval_00, eval_01…)
        // for deterministic ToolAt / GuardAt index assertions.
        let mut pairs: Vec<(String, crate::state::ToolCallRecord)> = working_memory
            .with_records(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .await;
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let tool_calls = pairs.into_iter().map(|(_, v)| v).collect();

        let trace = SessionTrace {
            conv_id:           String::new(), // eval traces are not tied to a real conversation
            started_at:        0,
            ended_at:          0,
            round_count:       1, // single simulated round
            skill_activations: vec![],
            llm_latency_ms:    vec![],
            tool_calls,
        };

        EvalResult::evaluate(trace, &case.assertions)
    }
}

/// Load all `status = "enabled"` eval cases from `proposed_eval_cases` table,
/// run each through [`EvalRunner::run`], and return `(case_name, EvalResult)` pairs.
pub(crate) async fn load_enabled_cases(
    db: &SurrealDb,
    account_id: &str,
) -> Vec<(String, EvalResult)> {
    let mut resp = match db
        .query("SELECT * FROM proposed_eval_cases WHERE account_id = $aid AND status = 'enabled'")
        .bind(("aid", account_id.to_string()))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[eval] load_enabled_cases query error: {}", e);
            return vec![];
        }
    };

    let rows: Vec<Value> = resp.take(0).unwrap_or_default();
    let mut results = Vec::with_capacity(rows.len());

    for row in rows {
        let name = row["name"].as_str().unwrap_or("unnamed").to_string();
        let case: EvalCase = match serde_json::from_value(row.clone()) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[eval] deserialise case '{}' failed: {}", name, e);
                continue;
            }
        };
        let result = EvalRunner::run(&case).await;
        results.push((name, result));
    }

    results
}

/// Build an eval-specific `ToolRegistry`.
///
/// For each entry in `tool_sequence`:
/// - Look up the real `ToolGuardSpec` from `ALL_TOOL_DEFS` (same source as production).
/// - If the guard blocks → return the `__guard_blocked__` sentinel (Executor detects it).
/// - If the guard passes (or no guard) → dequeue and return the predefined `mock_result`.
///
/// Multiple calls to the same tool name dequeue results in sequence — the first call
/// gets the first matching `mock_result`, the second call gets the second, etc.
fn build_eval_registry(
    tool_sequence: &[MockToolCall],
    working_memory: WorkingMemory,
) -> ToolRegistry {
    // Shared result queue: (tool_name, mock_result) in call order.
    let queue: Arc<tokio::sync::Mutex<VecDeque<(String, Value)>>> = Arc::new(
        tokio::sync::Mutex::new(
            tool_sequence.iter()
                .map(|tc| (tc.name.clone(), tc.mock_result.clone()))
                .collect(),
        )
    );

    let mut registry = ToolRegistry::new();
    let mut registered: std::collections::HashSet<String> = std::collections::HashSet::new();

    for mock_call in tool_sequence {
        if !registered.insert(mock_call.name.clone()) {
            continue; // already registered; the queue handles multiple calls
        }

        let name  = mock_call.name.clone();
        let wm    = working_memory.clone();
        let queue = Arc::clone(&queue);
        let guard = find_tool_def(&name).and_then(|d| d.guard);

        let execute: crate::service_agent::types::ToolFn = Arc::new(move |args: Value| {
            let wm    = wm.clone();
            let queue = Arc::clone(&queue);
            let name  = name.clone();
            Box::pin(async move {
                // Apply real guard logic (identical to build_interactive_registry).
                if let Some(ref spec) = guard {
                    let hint = wm.with_records(|store| evaluate_guard(spec, &args, store)).await;
                    if let Some(h) = hint {
                        return Ok(json!({ "__guard_blocked__": true, "__guard_hint__": h }));
                    }
                }
                // Dequeue the first result for this tool name.
                let mut q = queue.lock().await;
                let result = q.iter().position(|(n, _)| n == &name)
                    .map(|i| { let (_, r) = q.remove(i).unwrap(); r })
                    .unwrap_or_else(|| json!("eval_mock_result_exhausted"));
                Ok(result)
            })
        });

        registry.register(mock_call.name.clone(), Tool { execute, rollback: None });
    }

    registry
}
