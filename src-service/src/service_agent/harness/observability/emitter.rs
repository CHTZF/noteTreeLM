use std::sync::{Arc, Mutex};
use serde_json::Value;

use crate::service_agent::types::EmitEventFn;
use super::trace::SessionTrace;
use super::super::memory::working::WorkingMemory;

/// Internal mutable state accumulated during one agent session.
struct EmitterState {
    started_at:        i64,
    round_count:       u32,
    skill_activations: Vec<String>,
    llm_latency_ms:    Vec<u64>,
}

/// Wraps [`EmitEventFn`] to intercept SSE events and accumulate a [`SessionTrace`].
///
/// # Pattern
/// ```
/// let emitter = ObservabilityEmitter::new(raw_emit_fn);
/// let emit_fn: EmitEventFn = emitter.as_emit_fn();  // pass to Dispatcher / LLM helpers
///
/// // Inside the round loop:
/// emitter.increment_round();
/// emitter.record_llm_latency(ms);
///
/// // After session ends:
/// let trace = emitter.finish(&working_memory).await;
/// ```
///
/// `ObservabilityEmitter` is cheaply cloneable — all clones share the same backing state.
/// The `as_emit_fn()` closure also shares it, so all routes to emit are observed.
#[derive(Clone)]
pub(crate) struct ObservabilityEmitter {
    inner: EmitEventFn,
    state: Arc<Mutex<EmitterState>>,
}

impl ObservabilityEmitter {
    /// Returns the underlying raw emit function (without session_id stamp).
    /// Used by `fork_for_sub_agent` to build a new emitter with a fresh session_id.
    pub(crate) fn raw_emit_fn(&self) -> &EmitEventFn { &self.inner }

    pub(crate) fn new(inner: EmitEventFn) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(EmitterState {
                started_at:        chrono::Utc::now().timestamp(),
                round_count:       0,
                skill_activations: Vec::new(),
                llm_latency_ms:    Vec::new(),
            })),
        }
    }

    /// Returns an [`EmitEventFn`]-compatible closure that routes through this emitter.
    ///
    /// The closure intercepts `agent:skills_activated` to record activated skill titles
    /// into the session trace. All events are forwarded to the inner emit fn unchanged.
    pub(crate) fn as_emit_fn(&self) -> EmitEventFn {
        let me = self.clone();
        Arc::new(move |event: String, payload: Value| {
            me.emit(event, payload);
        })
    }

    /// Forward an event and record observable side-effects into the session trace.
    pub(crate) fn emit(&self, event: String, payload: Value) {
        if event == "agent:skills_activated" {
            if let Some(titles) = payload["titles"].as_array() {
                if let Ok(mut s) = self.state.lock() {
                    for t in titles {
                        if let Some(title) = t.as_str() {
                            s.skill_activations.push(title.to_string());
                        }
                    }
                }
            }
        }
        (self.inner)(event, payload);
    }

    /// Increment the LLM round counter. Call once at the top of each ReAct iteration
    /// and once for the pure-LLM path.
    pub(crate) fn increment_round(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.round_count += 1;
        }
    }

    /// Record the wall-clock latency for a single LLM call (milliseconds).
    pub(crate) fn record_llm_latency(&self, ms: u64) {
        if let Ok(mut s) = self.state.lock() {
            s.llm_latency_ms.push(ms);
        }
    }

    /// Bulk-append skill activation titles.
    ///
    /// Called by `run_interactive_agent` with titles discovered during the pre-pass
    /// skill search in `run_agent`. The pre-pass fires before the emitter is wired
    /// into Dispatcher, so `agent:skills_activated` events are not intercepted via
    /// `as_emit_fn()` — they must be injected here instead.
    pub(crate) fn record_skill_activations(&self, titles: &[String]) {
        if titles.is_empty() { return; }
        if let Ok(mut s) = self.state.lock() {
            s.skill_activations.extend_from_slice(titles);
        }
    }

    /// Assemble the final [`SessionTrace`] from accumulated state + `WorkingMemory`.
    ///
    /// Collects all `ToolCallRecord` entries (with timing + guard outcomes) from
    /// `working_memory` and combines them with the emitter-tracked metadata.
    pub(crate) async fn finish(
        &self,
        working_memory: &WorkingMemory,
        conv_id: &str,
        memory_facts_injected: usize,
    ) -> SessionTrace {
        let (started_at, round_count, skill_activations, llm_latency_ms) = {
            let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            (s.started_at, s.round_count, s.skill_activations.clone(), s.llm_latency_ms.clone())
        };

        let tool_calls: Vec<_> = working_memory
            .with_records(|map| map.values().cloned().collect())
            .await;
        let repeated_call_count = working_memory.repeated_calls().await.len();

        SessionTrace {
            conv_id: conv_id.to_string(),
            started_at,
            ended_at: chrono::Utc::now().timestamp(),
            round_count,
            skill_activations,
            llm_latency_ms,
            tool_calls,
            memory_facts_injected,
            repeated_call_count,
        }
    }
}
