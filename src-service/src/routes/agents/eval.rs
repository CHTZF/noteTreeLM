/// Eval harness routes — propose, manage, and run eval cases.
///
/// | Method | Path                                      | Purpose                    |
/// |--------|-------------------------------------------|----------------------------|
/// | POST   | /vaults/:vid/eval/trace-analysis          | Run trace_analyst agent    |
/// | GET    | /vaults/:vid/eval/cases                   | List proposed_eval_cases   |
/// | PATCH  | /vaults/:vid/eval/cases/:case_id/toggle   | Enable / disable a case    |
/// | POST   | /vaults/:vid/eval/run                     | Run all enabled cases      |

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde_json::{json, Value};
use uuid::Uuid;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use crate::app_state::ApiState;
use super::account_id_from_headers;

// ── Per-account trace_analyst run lock ───────────────────────────────────────

/// In-memory set of account_ids that currently have a trace_analyst running.
/// Prevents duplicate background runs from concurrent button clicks.
fn running_analyses() -> &'static Mutex<HashSet<String>> {
    static CELL: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashSet::new()))
}

// ── POST /vaults/:vid/eval/trace-analysis ─────────────────────────────────────

/// Trigger a trace_analyst agent run to analyse recent sessions and propose eval cases.
/// Returns immediately with session_id; analysis runs in background.
/// Returns 409 if an analysis is already running for this account.
pub async fn run_trace_analysis(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;

    // Reject if an analysis is already in progress for this account.
    {
        let mut running = running_analyses().lock().unwrap();
        if running.contains(&account_id) {
            return Err((StatusCode::CONFLICT, "trace_analyst already running for this account".to_string()));
        }
        running.insert(account_id.clone());
    }

    let session_id = Uuid::new_v4().to_string();

    let agent_def = {
        let mut def = crate::service::helpers::load_agent_def(
            &state.db, "trace_analyst", &account_id,
        )
        .await
        .ok_or((StatusCode::NOT_FOUND, "trace_analyst agent not found — run seed-builtins first".to_string()))?;
        def["session_id"] = json!(session_id.clone());
        def
    };

    let input = body["input"].as_str()
        .unwrap_or("請分析最近的 session traces，找出值得新增的 eval cases 並提案。")
        .to_string();

    let conversation_id = body["conversation_id"].as_str()
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let runtime = match state.agent_runtime(
        &vault_id, &account_id,
        Some(session_id.clone()),
        conversation_id,
        agent_def,
        false, // silent background run — no SSE streaming
    ).await {
        Some(r) => r,
        None => return Ok(Json(json!({ "error": "LLM not configured" }))),
    };

    tokio::spawn(async move {
        let db = runtime.db.clone();
        crate::service::agents::agent::run_agent(
            runtime,
            input,
            None,
        ).await;

        // Auto-run enabled eval cases after trace_analyst finishes, so last_run_result
        // stays fresh without requiring a manual "Run All" click.
        let enabled_count: i64 = db
            .query("SELECT count() AS count FROM proposed_eval_cases \
                    WHERE account_id = $aid AND status = 'enabled' GROUP ALL")
            .bind(("aid", account_id.clone()))
            .await
            .ok()
            .and_then(|mut r| {
                #[derive(serde::Deserialize)] struct C { count: i64 }
                r.take::<Vec<C>>(0).ok()?.into_iter().next().map(|c| c.count)
            })
            .unwrap_or(0);

        if enabled_count > 0 {
            let results = crate::service::harness::eval::runner::load_enabled_cases(
                &db,
                &account_id,
            ).await;

            let now = chrono::Utc::now().timestamp();
            for (name, result) in &results {
                let result_str = if result.passed() { "passed" } else { "failed" };
                let _ = db
                    .query(
                        "UPDATE proposed_eval_cases SET last_run_result = $res, last_run_at = $now \
                         WHERE account_id = $aid AND name = $name AND status = 'enabled'"
                    )
                    .bind(("res",  result_str.to_string()))
                    .bind(("now",  now))
                    .bind(("aid",  account_id.clone()))
                    .bind(("name", name.clone()))
                    .await;
            }

            let passed = results.iter().filter(|(_, r)| r.passed()).count();
            tracing::info!(
                "[eval] auto-run after trace_analyst: {}/{} passed",
                passed, results.len()
            );
        } else {
            tracing::debug!("[eval] auto-run skipped — no enabled cases for account");
        }

        // Release lock so the account can trigger another analysis.
        running_analyses().lock().unwrap().remove(&account_id);
    });

    Ok(Json(json!({ "session_id": session_id })))
}

// ── GET /vaults/:vid/eval/cases ───────────────────────────────────────────────

/// List all proposed eval cases for this account (all statuses).
pub async fn list_eval_cases(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(_vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;

    let mut resp = state.db
        .query(
            "SELECT meta::id(id) AS case_id, name, description, source, status, \
             last_run_result, last_run_at, array::len(tool_sequence) AS step_count \
             FROM proposed_eval_cases WHERE account_id = $aid \
             ORDER BY source ASC, name ASC"
        )
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let cases: Vec<Value> = resp.take(0).unwrap_or_default();
    Ok(Json(json!({ "cases": cases })))
}

// ── GET /vaults/:vid/eval/cases/:case_id ─────────────────────────────────────

/// Return the full eval case record including tool_sequence and assertions.
pub async fn get_eval_case(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((_vault_id, case_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;

    let mut resp = state.db
        .query(
            "SELECT meta::id(id) AS case_id, name, description, source, status, \
             tool_sequence, assertions, source_trace_ids, last_run_result, last_run_at \
             FROM proposed_eval_cases \
             WHERE meta::id(id) = $cid AND account_id = $aid \
             LIMIT 1"
        )
        .bind(("cid", case_id.clone()))
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp.take(0).unwrap_or_default();
    rows.into_iter().next()
        .map(|v| Json(v))
        .ok_or((StatusCode::NOT_FOUND, format!("eval case '{}' not found", case_id)))
}

// ── PATCH /vaults/:vid/eval/cases/:case_id/toggle ────────────────────────────

/// Enable or disable an eval case. Body: { "enabled": true | false }
/// Also accepts "pending_review" → "enabled" promotion.
pub async fn toggle_eval_case(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((_vault_id, case_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let enabled = body["enabled"].as_bool().unwrap_or(false);
    let new_status = if enabled { "enabled" } else { "disabled" };

    state.db
        .query(
            "UPDATE proposed_eval_cases SET status = $status \
             WHERE meta::id(id) = $cid AND account_id = $aid"
        )
        .bind(("status", new_status.to_string()))
        .bind(("cid",    case_id.clone()))
        .bind(("aid",    account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "case_id": case_id, "status": new_status })))
}

// ── DELETE /vaults/:vid/eval/cases/:case_id ──────────────────────────────────

/// Delete an eval case permanently (e.g. bad proposals from trace_analyst).
pub async fn delete_eval_case(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((_vault_id, case_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;

    state.db
        .query(
            "DELETE proposed_eval_cases \
             WHERE meta::id(id) = $cid AND account_id = $aid"
        )
        .bind(("cid", case_id.clone()))
        .bind(("aid", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "case_id": case_id, "deleted": true })))
}

// ── POST /vaults/:vid/eval/run ────────────────────────────────────────────────

/// Run all enabled eval cases and return results.
/// Each result: { name, passed, failures: [string] }
pub async fn run_eval_suite(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(_vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;

    let results = crate::service::harness::eval::runner::load_enabled_cases(
        &state.db,
        &account_id,
    ).await;

    let now = chrono::Utc::now().timestamp();
    let output: Vec<Value> = results.iter().map(|(name, r)| {
        json!({
            "name":        name,
            "description": r.description,
            "passed":      r.passed(),
            "failures":    r.failures,
        })
    }).collect();

    // Update last_run_result + last_run_at for each case
    for (name, result) in &results {
        let result_str = if result.passed() { "passed" } else { "failed" };
        let _ = state.db
            .query(
                "UPDATE proposed_eval_cases SET last_run_result = $res, last_run_at = $now \
                 WHERE account_id = $aid AND name = $name AND status = 'enabled'"
            )
            .bind(("res",  result_str.to_string()))
            .bind(("now",  now))
            .bind(("aid",  account_id.clone()))
            .bind(("name", name.clone()))
            .await;
    }

    let total  = output.len();
    let passed = output.iter().filter(|r| r["passed"].as_bool().unwrap_or(false)).count();

    Ok(Json(json!({
        "total":   total,
        "passed":  passed,
        "failed":  total - passed,
        "results": output,
    })))
}
