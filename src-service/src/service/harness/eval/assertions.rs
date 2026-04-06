use serde::{Deserialize, Serialize};
use crate::service::harness::governance::guard::GuardOutcome;
use crate::service::harness::observability::trace::SessionTrace;

/// Assertion kinds for evaluating a [`SessionTrace`].
///
/// **Layer 2 (behavioral):**
/// - `NoBlockedCalls`, `BlockedCountEq`, `GuardAt`, `ToolAt`, `TotalCallsEq`
///
/// **Layer 2b (stall detection):**
/// - `NoRepeatedCalls`, `RepeatedCallCountEq`
///
/// **Layer 3 (performance budget):**
/// - `RoundCountLe`, `TotalToolMsLe`, `LlmLatencyMeanLe`
///
/// Serialised with adjacently-tagged format: `{"type": "BlockedCountEq", "value": 2}`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub(crate) enum TraceAssertion {
    // ── Layer 2: behavioral ──────────────────────────────────────────────────

    /// All tool calls must have `Passed` or `Exempt` guard outcome.
    NoBlockedCalls,

    /// Exactly `n` calls must have `Blocked` guard outcome.
    BlockedCountEq(usize),

    /// Total recorded tool calls must equal `n`.
    TotalCallsEq(usize),

    /// The call at `index` (ordered by execution sequence) must have this guard outcome.
    GuardAt { index: usize, expected: GuardOutcomeKind },

    /// The call at `index` must have this tool name.
    ToolAt { index: usize, name: String },

    // ── Layer 2b: stall detection ────────────────────────────────────────────

    /// No (tool_name, arg) pair should appear ≥ 2 times — stall warning must not trigger.
    NoRepeatedCalls,

    /// Exactly `n` distinct (tool_name, arg) pairs were called ≥ 2 times.
    RepeatedCallCountEq(usize),

    // ── Layer 3: performance budget ──────────────────────────────────────────

    /// `trace.round_count` must be ≤ `n`.
    RoundCountLe(u32),

    /// Sum of all tool durations (`total_tool_ms()`) must be ≤ `ms`.
    TotalToolMsLe(u64),

    /// Mean per-round LLM latency must be ≤ `ms`.
    /// Skipped (passes) when `llm_latency_ms` is empty.
    LlmLatencyMeanLe(u64),
}

/// Simplified guard outcome kind for use in assertions.
/// Avoids owning the hint `String` inside `GuardOutcome::Blocked`.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum GuardOutcomeKind {
    Passed,
    Blocked,
    Exempt,
}

impl From<&GuardOutcome> for GuardOutcomeKind {
    fn from(g: &GuardOutcome) -> Self {
        match g {
            GuardOutcome::Passed     => Self::Passed,
            GuardOutcome::Blocked(_) => Self::Blocked,
            GuardOutcome::Exempt     => Self::Exempt,
        }
    }
}

/// Result of running an [`EvalCase`].
#[derive(Serialize, Deserialize)]
pub(crate) struct EvalResult {
    /// Case description, surfaced in run output for easier debugging.
    pub description: String,
    /// The assembled session trace.
    pub trace:       SessionTrace,
    /// Assertion failures — empty means all passed.
    pub failures:    Vec<String>,
}

impl EvalResult {
    /// Returns true when all assertions passed.
    pub(crate) fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// Evaluate all assertions against `trace`, collecting failure messages.
    pub(crate) fn evaluate(description: String, trace: SessionTrace, assertions: &[TraceAssertion]) -> Self {
        let failures = assertions.iter()
            .filter_map(|a| check(&trace, a))
            .collect();
        Self { description, trace, failures }
    }
}

fn check(trace: &SessionTrace, assertion: &TraceAssertion) -> Option<String> {
    match assertion {
        TraceAssertion::NoBlockedCalls => {
            let n = trace.blocked_calls();
            if n > 0 { Some(format!("NoBlockedCalls: {} blocked call(s) found", n)) }
            else { None }
        }

        TraceAssertion::BlockedCountEq(expected) => {
            let n = trace.blocked_calls();
            if n != *expected {
                Some(format!("BlockedCountEq({}): got {}", expected, n))
            } else { None }
        }

        TraceAssertion::TotalCallsEq(expected) => {
            let n = trace.total_calls();
            if n != *expected {
                Some(format!("TotalCallsEq({}): got {}", expected, n))
            } else { None }
        }

        TraceAssertion::GuardAt { index, expected } => {
            match trace.tool_calls.get(*index) {
                None => Some(format!(
                    "GuardAt({}): index out of range (trace has {} calls)",
                    index, trace.tool_calls.len()
                )),
                Some(rec) => {
                    let got = GuardOutcomeKind::from(&rec.guard_outcome);
                    if got != *expected {
                        Some(format!(
                            "GuardAt({})[{}]: expected {:?}, got {:?}",
                            index, rec.name, expected, got
                        ))
                    } else { None }
                }
            }
        }

        TraceAssertion::ToolAt { index, name } => {
            match trace.tool_calls.get(*index) {
                None => Some(format!(
                    "ToolAt({}): index out of range (trace has {} calls)",
                    index, trace.tool_calls.len()
                )),
                Some(rec) => {
                    if rec.name != *name {
                        Some(format!(
                            "ToolAt({}): expected '{}', got '{}'",
                            index, name, rec.name
                        ))
                    } else { None }
                }
            }
        }

        TraceAssertion::NoRepeatedCalls => {
            if trace.repeated_call_count > 0 {
                Some(format!("NoRepeatedCalls: {} repeated (tool, arg) pair(s) found", trace.repeated_call_count))
            } else { None }
        }

        TraceAssertion::RepeatedCallCountEq(expected) => {
            if trace.repeated_call_count != *expected {
                Some(format!("RepeatedCallCountEq({}): got {}", expected, trace.repeated_call_count))
            } else { None }
        }

        TraceAssertion::RoundCountLe(max) => {
            if trace.round_count > *max {
                Some(format!("RoundCountLe({}): got {}", max, trace.round_count))
            } else { None }
        }

        TraceAssertion::TotalToolMsLe(max) => {
            let total = trace.total_tool_ms();
            if total > *max {
                Some(format!("TotalToolMsLe({}ms): got {}ms", max, total))
            } else { None }
        }

        TraceAssertion::LlmLatencyMeanLe(max) => {
            if trace.llm_latency_ms.is_empty() { return None; }
            let mean = trace.llm_latency_ms.iter().sum::<u64>()
                / trace.llm_latency_ms.len() as u64;
            if mean > *max {
                Some(format!("LlmLatencyMeanLe({}ms): mean={}ms", max, mean))
            } else { None }
        }
    }
}

#[cfg(test)]
mod serde_tests {
    use super::*;
    #[test]
    fn trace_assertion_roundtrip() {
        let cases = vec![
            TraceAssertion::NoBlockedCalls,
            TraceAssertion::BlockedCountEq(2),
            TraceAssertion::TotalCallsEq(3),
            TraceAssertion::GuardAt { index: 0, expected: GuardOutcomeKind::Blocked },
            TraceAssertion::ToolAt { index: 1, name: "update_note".to_string() },
            TraceAssertion::RoundCountLe(5),
        ];
        for a in &cases {
            let s = serde_json::to_string(a).expect("serialize");
            let back: TraceAssertion = serde_json::from_str(&s).expect(&format!("deserialize: {}", s));
            let s2 = serde_json::to_string(&back).unwrap();
            assert_eq!(s, s2, "roundtrip failed");
        }
    }
}
