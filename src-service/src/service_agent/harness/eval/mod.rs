/// Eval & Test Harness — Layers 1-3.
///
/// | Layer | What is tested                            | How                         |
/// |-------|-------------------------------------------|-----------------------------|
/// | 1     | Guard regression (`evaluate_guard`)       | Unit tests in `guard.rs`    |
/// | 2     | Tool sequence + guard + working_memory    | `EvalRunner` + mock registry|
/// | 3     | Performance budget on SessionTrace        | `TraceAssertion` thresholds |
///
/// The entire module is `#[cfg(test)]`-only: it compiles only for `cargo test`
/// and has zero cost in release builds.

pub(crate) mod assertions;
pub(crate) mod runner;
#[cfg(test)]
mod cases;

pub(crate) use assertions::{EvalResult, GuardOutcomeKind, TraceAssertion};
pub(crate) use runner::EvalRunner;

use serde_json::Value;

/// A single tool call the mock LLM would produce, paired with its predefined result.
///
/// `mock_result` bypasses real I/O — the eval registry returns it directly after the
/// guard passes. If the guard blocks, the sentinel is returned instead and the
/// `mock_result` is never used.
#[derive(Clone)]
pub(crate) struct MockToolCall {
    /// Optional LLM-assigned call id. Leave empty to auto-generate "eval_NN".
    pub id:          String,
    /// Tool name — must match an entry in ALL_TOOL_DEFS for the guard to apply.
    pub name:        String,
    /// Arguments passed to the tool (and to the guard path_extractor).
    pub args:        Value,
    /// What this tool handler returns when the guard passes.
    pub mock_result: Value,
}

impl MockToolCall {
    pub(crate) fn new(name: &str, args: Value, mock_result: Value) -> Self {
        Self { id: String::new(), name: name.to_string(), args, mock_result }
    }
}

/// A complete eval scenario: a named sequence of mock tool calls + assertions on the trace.
pub(crate) struct EvalCase {
    pub name:          &'static str,
    pub tool_sequence: Vec<MockToolCall>,
    pub assertions:    Vec<TraceAssertion>,
}
