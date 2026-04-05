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
            name:        "read_note→update_note happy path".to_string(),
            description: "Guard should pass when update_note is preceded by a successful read_note on the same path.".to_string(),
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
            seed_version: 1,
        },

        EvalCase {
            name:        "update_note without prerequisite — blocked".to_string(),
            description: "Guard should block update_note when no prior read_note or search result exists for the target path.".to_string(),
            tool_sequence: vec![
                MockToolCall::new("update_note", json!({"path": "notes/daily.md", "content": "new content"}), json!("✅ 更新完成")),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::TotalCallsEq(1),
                TraceAssertion::GuardAt { index: 0, expected: GuardOutcomeKind::Blocked },
            ],
            seed_version: 1,
        },

        EvalCase {
            name:        "update_note after failed read_note — blocked".to_string(),
            description: "Guard should block update_note even when read_note was called but returned an error (content not successfully read).".to_string(),
            tool_sequence: vec![
                MockToolCall::new("read_note",   json!({"path": "notes/daily.md"}), json!("讀取失敗：file not found")),
                MockToolCall::new("update_note", json!({"path": "notes/daily.md", "content": "new content"}), json!("✅ 更新完成")),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Blocked },
            ],
            seed_version: 1,
        },

        EvalCase {
            name:        "search_vault→delete_note happy path".to_string(),
            description: "Guard should pass when delete_note is preceded by a search_vault that returned the target path.".to_string(),
            tool_sequence: vec![
                MockToolCall::new("search_vault", json!({"query": "old notes"}), json!([{"path": "archive/old.md", "content": "…"}])),
                MockToolCall::new("delete_note",  json!({"path": "archive/old.md"}), json!("✅ 刪除完成")),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::TotalCallsEq(2),
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Passed },
            ],
            seed_version: 1,
        },

        EvalCase {
            name:        "delete_note without prior evidence — blocked".to_string(),
            description: "Guard should block delete_note when no search or read has established the path exists.".to_string(),
            tool_sequence: vec![
                MockToolCall::new("delete_note", json!({"path": "archive/old.md"}), json!("✅ 刪除完成")),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::GuardAt { index: 0, expected: GuardOutcomeKind::Blocked },
            ],
            seed_version: 1,
        },

        EvalCase {
            name:        "list_structure→delete_folder happy path".to_string(),
            description: "Guard should pass when delete_folder is preceded by list_structure that showed the folder exists.".to_string(),
            tool_sequence: vec![
                MockToolCall::new("list_structure", json!({}), json!("vault/\n  archive/\n    archive/old.md")),
                MockToolCall::new("delete_folder",  json!({"path": "archive"}), json!("✅ 資料夾刪除完成")),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::TotalCallsEq(2),
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Passed },
            ],
            seed_version: 1,
        },

        EvalCase {
            name:        "create_note is exempt (no guard required)".to_string(),
            description: "create_note is a creation tool — it should be guard-exempt and never blocked regardless of prior reads.".to_string(),
            tool_sequence: vec![
                MockToolCall::new("create_note", json!({"path": "notes/new.md", "content": "# New Note"}), json!("✅ 建立完成")),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::TotalCallsEq(1),
                TraceAssertion::GuardAt { index: 0, expected: GuardOutcomeKind::Exempt },
            ],
            seed_version: 1,
        },

        EvalCase {
            name:        "search_vault alone — update_note ContentRead not satisfied".to_string(),
            description: "search_vault provides path evidence but not full content read — guard should still block update_note.".to_string(),
            tool_sequence: vec![
                MockToolCall::new("search_vault", json!({"query": "target"}), json!([{"path": "notes/target.md", "content": "preview"}])),
                MockToolCall::new("update_note",  json!({"path": "notes/target.md", "content": "new content"}), json!("✅ 更新完成")),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::GuardAt { index: 1, expected: GuardOutcomeKind::Blocked },
            ],
            seed_version: 1,
        },

        // ── Layer 3: performance budget ──────────────────────────────────────

        EvalCase {
            name:        "Layer 3 — tool execution time within budget".to_string(),
            description: "A normal read→update sequence should complete within the 5s tool budget and 3 round cap.".to_string(),
            tool_sequence: vec![
                MockToolCall::new("read_note",   json!({"path": "notes/ref.md"}), json!("# Ref\ncontent")),
                MockToolCall::new("update_note", json!({"path": "notes/ref.md", "content": "updated"}), json!("✅ 更新完成")),
            ],
            assertions: vec![
                TraceAssertion::NoBlockedCalls,
                TraceAssertion::RoundCountLe(3),
                TraceAssertion::TotalToolMsLe(5_000),
            ],
            seed_version: 1,
        },

        EvalCase {
            name:        "Layer 3 — guard block is fast".to_string(),
            description: "Guard evaluation itself should be near-instantaneous (< 1s) even for blocked calls.".to_string(),
            tool_sequence: vec![
                MockToolCall::new("delete_note", json!({"path": "notes/important.md"}), json!("✅ 刪除完成")),
            ],
            assertions: vec![
                TraceAssertion::BlockedCountEq(1),
                TraceAssertion::TotalToolMsLe(1_000),
            ],
            seed_version: 1,
        },
    ]
}

/// Upsert all seed eval cases into `proposed_eval_cases` table.
///
/// - If a case with the same name doesn't exist → INSERT (status defaults to "enabled").
/// - If a case exists AND its stored `seed_version` < code `seed_version` → UPDATE content
///   fields (tool_sequence, assertions, description, seed_version) while preserving status.
///   This mirrors the `system_prompt_version` mechanism used for agent_definitions.
pub(crate) async fn seed_eval_cases(db: &SurrealDb, account_id: &str) {
    for case in seed_cases() {
        // Query existing row: id + seed_version (defaults to 0 if field absent).
        #[derive(serde::Deserialize)]
        struct ExistingRow { id: serde_json::Value, #[serde(default)] seed_version: u32 }
        let mut resp = match db
            .query("SELECT id, seed_version FROM proposed_eval_cases WHERE account_id = $aid AND name = $name LIMIT 1")
            .bind(("aid",  account_id.to_string()))
            .bind(("name", case.name.clone()))
            .await
        {
            Ok(r) => r,
            Err(e) => { tracing::warn!("[seed_eval_cases] query '{}' error: {}", case.name, e); continue; }
        };
        let existing: Vec<ExistingRow> = resp.take(0).unwrap_or_default();

        if existing.is_empty() {
            // INSERT new case.
            let payload = match serde_json::to_value(&case) {
                Ok(mut v) => {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("account_id".to_string(), json!(account_id));
                        obj.insert("source".to_string(),     json!("seed"));
                        obj.insert("status".to_string(),     json!("enabled"));
                        obj.insert("last_run_result".to_string(), serde_json::Value::Null);
                        obj.insert("last_run_at".to_string(),     serde_json::Value::Null);
                    }
                    v
                }
                Err(e) => { tracing::warn!("[seed_eval_cases] serialise '{}' failed: {}", case.name, e); continue; }
            };
            if let Err(e) = db.query("CREATE proposed_eval_cases CONTENT $data")
                .bind(("data", payload))
                .await
            {
                tracing::warn!("[seed_eval_cases] insert '{}' error: {}", case.name, e);
            }
        } else if existing[0].seed_version < case.seed_version {
            // UPDATE content fields; preserve status and last_run fields.
            let row_id = &existing[0].id;
            let tool_seq = serde_json::to_value(&case.tool_sequence).unwrap_or_default();
            let assertions = serde_json::to_value(&case.assertions).unwrap_or_default();
            if let Err(e) = db
                .query("UPDATE $rid MERGE { description: $desc, tool_sequence: $ts, assertions: $as, seed_version: $ver }")
                .bind(("rid",  row_id.clone()))
                .bind(("desc", case.description.clone()))
                .bind(("ts",   tool_seq))
                .bind(("as",   assertions))
                .bind(("ver",  case.seed_version))
                .await
            {
                tracing::warn!("[seed_eval_cases] update '{}' error: {}", case.name, e);
            } else {
                tracing::info!("[seed_eval_cases] updated '{}' to seed_version={}", case.name, case.seed_version);
            }
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
