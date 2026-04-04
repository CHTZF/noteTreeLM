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

use crate::api_state::ApiState;
use super::account_id_from_headers;

// ── POST /vaults/:vid/eval/trace-analysis ─────────────────────────────────────

/// Trigger a trace_analyst agent run to analyse recent sessions and propose eval cases.
/// Returns immediately with session_id; analysis runs in background.
pub async fn run_trace_analysis(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let session_id = Uuid::new_v4().to_string();

    let agent_def = {
        let mut def = crate::service_agent::helpers::load_agent_def(
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

    let state_clone = state.clone();
    tokio::spawn(async move {
        crate::service_agent::agents::interactive::run_agent(
            state_clone,
            agent_def,
            input,
            vault_id,
            account_id,
            conversation_id,
            false, // silent background run — no SSE streaming
            None,
        ).await;
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

// ── POST /vaults/:vid/eval/run ────────────────────────────────────────────────

/// Run all enabled eval cases and return results.
/// Each result: { name, passed, failures: [string] }
pub async fn run_eval_suite(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(_vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;

    let results = crate::service_agent::harness::eval::runner::load_enabled_cases(
        &state.db,
        &account_id,
    ).await;

    let now = chrono::Utc::now().timestamp();
    let output: Vec<Value> = results.iter().map(|(name, r)| {
        json!({
            "name":     name,
            "passed":   r.passed(),
            "failures": r.failures,
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
