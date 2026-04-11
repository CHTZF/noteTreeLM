use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use crate::app_state::ApiState;
use crate::routes::auth::extract_bearer;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/calendar/connect",    post(connect_calendar))
        .route("/calendar/disconnect", post(disconnect_calendar))
        .route("/calendar/status",     get(calendar_status))
}

/// POST /calendar/connect
/// Body: { "refresh_token": "...", "client_id": "..." (optional — stored globally) }
///
/// Called by the frontend after completing Google OAuth with calendar scope.
/// Stores the refresh_token encrypted in user_settings.
/// Optionally stores client_id in global settings if provided.
async fn connect_calendar(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let refresh_token = body["refresh_token"].as_str()
        .ok_or((StatusCode::BAD_REQUEST, "Missing refresh_token".to_string()))?;

    // Persist google_client_id + google_client_secret to global settings
    let now = chrono::Utc::now().timestamp();
    if let Some(client_id) = body["client_id"].as_str().filter(|s| !s.is_empty()) {
        let _ = state.db
            .query("UPSERT settings SET `key` = 'google_client_id', `value` = $v, updated_at = $now \
                    WHERE `key` = 'google_client_id'")
            .bind(("v", client_id.to_string()))
            .bind(("now", now))
            .await;
    }
    if let Some(client_secret) = body["client_secret"].as_str().filter(|s| !s.is_empty()) {
        let _ = state.db
            .query("UPSERT settings SET `key` = 'google_client_secret', `value` = $v, updated_at = $now \
                    WHERE `key` = 'google_client_secret'")
            .bind(("v", client_secret.to_string()))
            .bind(("now", now))
            .await;
    }

    let ok = crate::service::harness::tools::calendar_tools::store_refresh_token(
        &state.db, &account_id, refresh_token,
    ).await;

    if ok {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err((StatusCode::INTERNAL_SERVER_ERROR, "Failed to store calendar token".to_string()))
    }
}

/// POST /calendar/disconnect
/// Removes the stored refresh_token for the current user.
async fn disconnect_calendar(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let now = chrono::Utc::now().timestamp();
    let _ = state.db
        .query("UPDATE user_settings SET `value` = '', updated_at = $now \
                WHERE username = $u AND `key` = 'google_calendar_refresh_token'")
        .bind(("u", account_id))
        .bind(("now", now))
        .await;
    Ok(Json(json!({ "ok": true })))
}

/// GET /calendar/status
/// Returns whether the current user has Google Calendar connected.
async fn calendar_status(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    #[derive(serde::Deserialize)]
    struct Row { value: String }
    let mut r = state.db
        .query("SELECT `value` FROM user_settings WHERE username = $u AND `key` = 'google_calendar_refresh_token' LIMIT 1")
        .bind(("u", account_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Row> = r.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let connected = rows.first().map(|r| !r.value.is_empty()).unwrap_or(false);
    Ok(Json(json!({ "connected": connected })))
}

async fn account_id_from_headers(
    state: &ApiState,
    headers: &HeaderMap,
) -> Result<String, (StatusCode, String)> {
    let token = extract_bearer(headers)
        .ok_or((StatusCode::UNAUTHORIZED, "Missing token".to_string()))?;
    let now = chrono::Utc::now().timestamp();
    #[derive(serde::Deserialize)]
    struct Row { username: String }
    let mut r = state.db
        .query("SELECT username FROM sessions WHERE token = $t AND expires_at > $now LIMIT 1")
        .bind(("t", token))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Row> = r.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    rows.into_iter().next().map(|r| r.username)
        .ok_or((StatusCode::UNAUTHORIZED, "Invalid or expired token".to_string()))
}
