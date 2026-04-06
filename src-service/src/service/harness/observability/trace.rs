use serde::{Deserialize, Serialize};
use crate::state::{GuardOutcome, ToolCallRecord};

/// Immutable snapshot of a single agent session's execution.
///
/// Built at session end by [`ObservabilityEmitter::finish`], combining
/// emitter-tracked metadata (round count, skill activations, LLM latency)
/// with the full tool-call evidence collected by [`WorkingMemory`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionTrace {
    /// Conversation ID linking this trace to its episodic message history.
    pub conv_id: String,
    /// Unix timestamp (seconds) when the agent session started.
    pub started_at: i64,
    /// Unix timestamp (seconds) when the session ended.
    pub ended_at: i64,
    /// Number of LLM rounds completed in this session.
    pub round_count: u32,
    /// Titles of skills activated during this session (from `agent:skills_activated` events).
    pub skill_activations: Vec<String>,
    /// Per-round LLM call latency in milliseconds (index 0 = first round).
    pub llm_latency_ms: Vec<u64>,
    /// All tool executions in this session, including guard-blocked calls.
    pub tool_calls: Vec<ToolCallRecord>,
    /// Number of memory facts injected into the context for this session.
    /// 0 means memory prefetch returned nothing or was skipped.
    pub memory_facts_injected: usize,
    /// Number of distinct (tool_name, arg) pairs called ≥ 2 times in this session.
    /// > 0 indicates the stall-detection warning was triggered.
    #[serde(default)]
    pub repeated_call_count: usize,
}

impl SessionTrace {
    /// Total number of tool calls recorded (includes guard-blocked).
    pub(crate) fn total_calls(&self) -> usize {
        self.tool_calls.len()
    }

    /// Number of calls blocked by a precondition guard.
    pub(crate) fn blocked_calls(&self) -> usize {
        self.tool_calls.iter()
            .filter(|r| matches!(r.guard_outcome, GuardOutcome::Blocked(_)))
            .count()
    }

    /// Sum of all tool execution durations in milliseconds.
    pub(crate) fn total_tool_ms(&self) -> u64 {
        self.tool_calls.iter().map(|r| r.duration_ms).sum()
    }
}
