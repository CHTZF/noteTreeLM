/// Seed eval cases — Layer 2 & 3 scenarios derived from core guard invariants.
///
/// Two entry points:
/// - `seed_eval_cases(db, account_id)`: upserts all cases into `proposed_eval_cases`
///   table at app startup. Status defaults to `"enabled"` so they run immediately.
/// - `#[tokio::test]` functions below: dev-time regression checks (cargo test).

use crate::db::SurrealDb;
use serde_json::json;

use super::{EvalCase, MockToolCall, TraceAssertion, GuardOutcomeKind};

/// Build the canonical list of seed eval cases.
fn seed_cases() -> Vec<EvalCase> {
    vec![
        // ── Layer 2: behavioral ──────────────────────────────────────────────

        EvalCase {
            name: "read_note→update_note happy path".to_string(),
            tool_sequence: vec![
                MockToolCall::new("read_note",   json!({"path": "notes/daily.md"}), json!("# Daily\nsome content")),
                MockToolCall::new("update_note", json!({"path": "notes/daily.md", "content": "# Daily\nupdated"}), json!("✅ 更新完成")),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::TotalCallsEq(2),
                TraceAssertion::ToolAt { index: 0, name: "read_note".to_string() },
                TraceAssertion::ToolAt { index: 1, name: "update_note".to_string() },
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Passed },
            ],
        },

        EvalCase {
            name: "update_note without prerequisite — blocked".to_string(),
            tool_sequence: vec![
                MockToolCall::new("update_note", json!({"path": "notes/daily.md", "content": "new content"}), json!("✅ 更新完成")),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::TotalCallsEq(1),
                TraceAssertion::GuardAt { index: 0, expected: GuardOutcomeKind::Blocked },
            ],
        },

        EvalCase {
            name: "update_note after failed read_note — blocked".to_string(),
            tool_sequence: vec![
                MockToolCall::new("read_note",   json!({"path": "notes/daily.md"}), json!("讀取失敗：file not found")),
                MockToolCall::new("update_note", json!({"path": "notes/daily.md", "content": "new content"}), json!("✅ 更新完成")),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Blocked },
            ],
        },

        EvalCase {
            name: "search_vault→delete_note happy path".to_string(),
            tool_sequence: vec![
                MockToolCall::new("search_vault", json!({"query": "old notes"}), json!([{"path": "archive/old.md", "content": "…"}])),
                MockToolCall::new("delete_note",  json!({"path": "archive/old.md"}), json!("✅ 刪除完成")),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::TotalCallsEq(2),
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Passed },
            ],
        },

        EvalCase {
            name: "delete_note without prior evidence — blocked".to_string(),
            tool_sequence: vec![
                MockToolCall::new("delete_note", json!({"path": "archive/old.md"}), json!("✅ 刪除完成")),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::GuardAt { index: 0, expected: GuardOutcomeKind::Blocked },
            ],
        },

        EvalCase {
            name: "list_structure→delete_folder happy path".to_string(),
            tool_sequence: vec![
                MockToolCall::new("list_structure", json!({}), json!("vault/\n  archive/\n    archive/old.md")),
                MockToolCall::new("delete_folder",  json!({"path": "archive"}), json!("✅ 資料夾刪除完成")),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::TotalCallsEq(2),
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Passed },
            ],
        },

        EvalCase {
            name: "create_note is exempt (no guard required)".to_string(),
            tool_sequence: vec![
                MockToolCall::new("create_note", json!({"path": "notes/new.md", "content": "# New Note"}), json!("✅ 建立完成")),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::TotalCallsEq(1),
                TraceAssertion::GuardAt { index: 0, expected: GuardOutcomeKind::Exempt },
            ],
        },

        EvalCase {
            name: "search_vault alone — update_note ContentRead not satisfied".to_string(),
            tool_sequence: vec![
                MockToolCall::new("search_vault", json!({"query": "target"}), json!([{"path": "notes/target.md", "content": "preview"}])),
                MockToolCall::new("update_note",  json!({"path": "notes/target.md", "content": "new content"}), json!("✅ 更新完成")),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Blocked },
            ],
        },

        // ── Layer 3: performance budget ──────────────────────────────────────

        EvalCase {
            name: "Layer 3 — tool execution time within budget".to_string(),
            tool_sequence: vec![
                MockToolCall::new("read_note",   json!({"path": "notes/ref.md"}), json!("# Ref\ncontent")),
                MockToolCall::new("update_note", json!({"path": "notes/ref.md", "content": "updated"}), json!("✅ 更新完成")),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::RoundCountLe(3),
                TraceAssertion::TotalToolMsLe(5_000),
            ],
        },

        EvalCase {
            name: "Layer 3 — guard block is fast".to_string(),
            tool_sequence: vec![
                MockToolCall::new("delete_note", json!({"path": "notes/important.md"}), json!("✅ 刪除完成")),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::TotalToolMsLe(1_000),
            ],
        },
    ]
}

/// Upsert all seed eval cases into `proposed_eval_cases` table.
///
/// Uses `name` as the idempotency key — if a case with the same name already
/// exists (regardless of status) it is left untouched, so manual status changes
/// made via the frontend are preserved across restarts.
pub(crate) async fn seed_eval_cases(db: &SurrealDb, account_id: &str) {
    for case in seed_cases() {
        let payload = match serde_json::to_value(&case) {
            Ok(mut v) => {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("account_id".to_string(),  json!(account_id));
                    obj.insert("source".to_string(),      json!("seed"));
                    obj.insert("status".to_string(),      json!("enabled"));
                    obj.insert("last_run_result".to_string(), serde_json::Value::Null);
                    obj.insert("last_run_at".to_string(),     serde_json::Value::Null);
                }
                v
            }
            Err(e) => {
                tracing::warn!("[seed_eval_cases] serialise '{}' failed: {}", case.name, e);
                continue;
            }
        };

        // INSERT only if no case with this name + account already exists.
        if let Err(e) = db
            .query(
                "IF (SELECT count() FROM proposed_eval_cases WHERE account_id = $aid AND name = $name GROUP ALL)[0].count = 0 \
                 THEN (CREATE proposed_eval_cases CONTENT $data) \
                 END"
            )
            .bind(("aid",  account_id.to_string()))
            .bind(("name", case.name.clone()))
            .bind(("data", payload))
            .await
        {
            tracing::warn!("[seed_eval_cases] upsert '{}' error: {}", case.name, e);
        }
    }
}

// ── Dev-time regression tests (cargo test only) ──────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_agent::harness::eval::EvalRunner;

    #[tokio::test]
    async fn all_seed_cases_pass() {
        for case in seed_cases() {
            let name = case.name.clone();
            let result = EvalRunner::run(&case).await;
            assert!(result.passed(), "seed case '{}' failed:\n{}", name, result.failures.join("\n"));
        }
    }
}
