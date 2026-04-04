/// Layer 2 & 3 eval cases — run through the real Executor + guard + WorkingMemory.
///
/// Each case is a named scenario describing what the "LLM" would call, with a
/// predefined mock result per tool. The EvalRunner wires real guard logic and
/// records execution in WorkingMemory exactly as production does.

#[cfg(test)]
mod tests {
    use serde_json::json;
    use crate::service_agent::harness::eval::{
        EvalCase, EvalRunner, MockToolCall, TraceAssertion, GuardOutcomeKind,
    };

    // ── Layer 2: Tool sequence + guard behavioral tests ───────────────────────

    /// Happy path: read_note → update_note.
    /// update_note requires ContentRead — read_note provides it.
    #[tokio::test]
    async fn read_note_then_update_note_passes() {
        let case = EvalCase {
            name: "read_note→update_note happy path",
            tool_sequence: vec![
                MockToolCall::new(
                    "read_note",
                    json!({"path": "notes/daily.md"}),
                    json!("# Daily\nsome content"),
                ),
                MockToolCall::new(
                    "update_note",
                    json!({"path": "notes/daily.md", "content": "# Daily\nupdated"}),
                    json!("✅ 更新完成"),
                ),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::TotalCallsEq(2),
                TraceAssertion::ToolAt { index: 0, name: "read_note" },
                TraceAssertion::ToolAt { index: 1, name: "update_note" },
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Passed },
            ],
        };
        let result = EvalRunner::run(&case).await;
        assert!(result.passed(), "case '{}' failed:\n{}", case.name, result.failures.join("\n"));
    }

    /// Guard blocks update_note when there is no prior read_note.
    #[tokio::test]
    async fn update_note_without_read_is_blocked() {
        let case = EvalCase {
            name: "update_note without prerequisite — blocked",
            tool_sequence: vec![
                MockToolCall::new(
                    "update_note",
                    json!({"path": "notes/daily.md", "content": "new content"}),
                    json!("✅ 更新完成"),
                ),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::TotalCallsEq(1),
                TraceAssertion::GuardAt { index: 0, expected: GuardOutcomeKind::Blocked },
            ],
        };
        let result = EvalRunner::run(&case).await;
        assert!(result.passed(), "case '{}' failed:\n{}", case.name, result.failures.join("\n"));
    }

    /// Guard blocks update_note even when read_note returned an error.
    /// An error result does NOT count as a successful ContentRead.
    #[tokio::test]
    async fn update_note_after_failed_read_is_blocked() {
        let case = EvalCase {
            name: "update_note after failed read_note — blocked",
            tool_sequence: vec![
                MockToolCall::new(
                    "read_note",
                    json!({"path": "notes/daily.md"}),
                    json!("讀取失敗：file not found"),  // error result
                ),
                MockToolCall::new(
                    "update_note",
                    json!({"path": "notes/daily.md", "content": "new content"}),
                    json!("✅ 更新完成"),
                ),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                // read_note runs (guard: None) → Passed/Exempt
                // update_note: path not seen via successful read → Blocked
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Blocked },
            ],
        };
        let result = EvalRunner::run(&case).await;
        assert!(result.passed(), "case '{}' failed:\n{}", case.name, result.failures.join("\n"));
    }

    /// search_vault then delete_note: PathSeen guard satisfied by search result.
    #[tokio::test]
    async fn search_vault_then_delete_note_passes() {
        let case = EvalCase {
            name: "search_vault→delete_note happy path",
            tool_sequence: vec![
                MockToolCall::new(
                    "search_vault",
                    json!({"query": "old notes"}),
                    json!([{"path": "archive/old.md", "content": "…"}]),
                ),
                MockToolCall::new(
                    "delete_note",
                    json!({"path": "archive/old.md"}),
                    json!("✅ 刪除完成"),
                ),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::TotalCallsEq(2),
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Passed },
            ],
        };
        let result = EvalRunner::run(&case).await;
        assert!(result.passed(), "case '{}' failed:\n{}", case.name, result.failures.join("\n"));
    }

    /// delete_note without prior search → PathSeen guard blocks.
    #[tokio::test]
    async fn delete_note_without_search_is_blocked() {
        let case = EvalCase {
            name: "delete_note without prior evidence — blocked",
            tool_sequence: vec![
                MockToolCall::new(
                    "delete_note",
                    json!({"path": "archive/old.md"}),
                    json!("✅ 刪除完成"),
                ),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::GuardAt { index: 0, expected: GuardOutcomeKind::Blocked },
            ],
        };
        let result = EvalRunner::run(&case).await;
        assert!(result.passed(), "case '{}' failed:\n{}", case.name, result.failures.join("\n"));
    }

    /// list_structure then delete_folder: PathSeen folder guard satisfied.
    #[tokio::test]
    async fn list_structure_then_delete_folder_passes() {
        let case = EvalCase {
            name: "list_structure→delete_folder happy path",
            tool_sequence: vec![
                MockToolCall::new(
                    "list_structure",
                    json!({}),
                    json!("vault/\n  archive/\n    archive/old.md"),
                ),
                MockToolCall::new(
                    "delete_folder",
                    json!({"path": "archive"}),
                    json!("✅ 資料夾刪除完成"),
                ),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::TotalCallsEq(2),
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Passed },
            ],
        };
        let result = EvalRunner::run(&case).await;
        assert!(result.passed(), "case '{}' failed:\n{}", case.name, result.failures.join("\n"));
    }

    /// Exempt tool (create_note) — no guard, records GuardOutcome::Exempt.
    #[tokio::test]
    async fn create_note_is_exempt() {
        let case = EvalCase {
            name: "create_note is exempt (no guard required)",
            tool_sequence: vec![
                MockToolCall::new(
                    "create_note",
                    json!({"path": "notes/new.md", "content": "# New Note"}),
                    json!("✅ 建立完成"),
                ),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::TotalCallsEq(1),
                TraceAssertion::GuardAt { index: 0, expected: GuardOutcomeKind::Exempt },
            ],
        };
        let result = EvalRunner::run(&case).await;
        assert!(result.passed(), "case '{}' failed:\n{}", case.name, result.failures.join("\n"));
    }

    /// search_vault only satisfies PathSeen but NOT ContentRead → update_note blocked.
    #[tokio::test]
    async fn search_vault_alone_does_not_satisfy_content_guard() {
        let case = EvalCase {
            name: "search_vault alone — update_note ContentRead not satisfied",
            tool_sequence: vec![
                MockToolCall::new(
                    "search_vault",
                    json!({"query": "target"}),
                    json!([{"path": "notes/target.md", "content": "preview"}]),
                ),
                MockToolCall::new(
                    "update_note",
                    json!({"path": "notes/target.md", "content": "new content"}),
                    json!("✅ 更新完成"),
                ),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Blocked },
            ],
        };
        let result = EvalRunner::run(&case).await;
        assert!(result.passed(), "case '{}' failed:\n{}", case.name, result.failures.join("\n"));
    }

    // ── Layer 3: Performance budget assertions ────────────────────────────────

    /// Mock tool execution completes well under 5 seconds total.
    #[tokio::test]
    async fn tool_execution_within_budget() {
        let case = EvalCase {
            name: "Layer 3 — tool execution time within budget",
            tool_sequence: vec![
                MockToolCall::new(
                    "read_note",
                    json!({"path": "notes/ref.md"}),
                    json!("# Ref\ncontent"),
                ),
                MockToolCall::new(
                    "update_note",
                    json!({"path": "notes/ref.md", "content": "updated"}),
                    json!("✅ 更新完成"),
                ),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::RoundCountLe(3),
                TraceAssertion::TotalToolMsLe(5_000),  // mock tools must complete in < 5s
            ],
        };
        let result = EvalRunner::run(&case).await;
        assert!(result.passed(), "case '{}' failed:\n{}", case.name, result.failures.join("\n"));
    }

    /// Blocked call still completes fast (guard evaluation is synchronous).
    #[tokio::test]
    async fn blocked_call_completes_fast() {
        let case = EvalCase {
            name: "Layer 3 — guard block is fast",
            tool_sequence: vec![
                MockToolCall::new(
                    "delete_note",
                    json!({"path": "notes/important.md"}),
                    json!("✅ 刪除完成"),
                ),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::TotalToolMsLe(1_000),  // guard eval < 1s
            ],
        };
        let result = EvalRunner::run(&case).await;
        assert!(result.passed(), "case '{}' failed:\n{}", case.name, result.failures.join("\n"));
    }
}
