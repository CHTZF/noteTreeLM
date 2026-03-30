use axum::{
    http::{HeaderMap, StatusCode},
    routing::{get, patch, post, put},
    Router,
};
use chrono::Utc;

use crate::api_state::ApiState;
use crate::routes::auth::extract_bearer;

mod definitions;
mod runner;
mod skills;
mod tools;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/agents", get(definitions::list).post(definitions::create))
        .route("/agents/:def_id", get(definitions::get_one).put(definitions::update).delete(definitions::delete))
        .route("/agents/:def_id/usage", post(definitions::record_usage))
        .route("/agents/:def_id/wake", post(definitions::wake))
        .route("/agents/:def_id/toggle", patch(definitions::toggle))
        .route("/agents/lifecycle", post(definitions::lifecycle))
        .route("/skills", get(skills::list).post(skills::create))
        .route("/skills/:skill_id", get(skills::get_one).put(skills::update).delete(skills::delete))
        .route("/skills/:skill_id/toggle", patch(skills::toggle))
        .route("/skills/:skill_id/trigger", post(skills::bump_trigger))
        .route("/skills/seed-builtins", post(skills::seed_builtins))
        .route("/agent-tools", get(tools::list).post(tools::create))
        .route("/agent-tools/:tool_id", put(tools::update).delete(tools::delete))
        // ── Interactive agent run/cancel/confirm ─────────────────────────────
        .route("/vaults/:vid/agent/run", post(runner::run))
        .route("/vaults/:vid/agent/cancel", post(runner::cancel))
        .route("/vaults/:vid/agent/confirm", post(runner::confirm))
        .route("/vaults/:vid/agent/live_chat", post(runner::live_chat))
}

/// Resolve account_id from Bearer token.
pub(super) async fn account_id_from_headers(
    state: &ApiState,
    headers: &HeaderMap,
) -> Result<String, (StatusCode, String)> {
    let token = extract_bearer(headers)
        .ok_or((StatusCode::UNAUTHORIZED, "Missing token".to_string()))?;
    let now = Utc::now().timestamp();
    #[derive(serde::Deserialize)]
    struct Row { username: String }
    let mut r = state.db
        .query("SELECT username FROM sessions WHERE token = $t AND expires_at > $now LIMIT 1")
        .bind(("t", token))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    r.take::<Vec<Row>>(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter().next()
        .map(|r| r.username)
        .ok_or((StatusCode::UNAUTHORIZED, "Invalid or expired session".to_string()))
}
