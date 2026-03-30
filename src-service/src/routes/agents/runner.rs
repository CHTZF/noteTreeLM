use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api_state::ApiState;
use super::account_id_from_headers;

/// POST /vaults/:vid/agent/run
/// Body: { session_id, input, messages, system, use_tools, activity_context, vault_path, conversation_id }
/// Spawns run_interactive_agent in background; immediately returns { session_id }.
pub async fn run(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let session_id = body["session_id"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let input = body["input"].as_str().unwrap_or("").to_string();
    let messages: Vec<Value> = body["messages"].as_array().cloned().unwrap_or_default();
    let system = body["system"].as_str().unwrap_or("").to_string();
    let use_tools = body["use_tools"].as_bool().unwrap_or(true);
    let activity_context = body["activity_context"].as_str().map(String::from);
    let vault_path = body["vault_path"].as_str().unwrap_or("").to_string();
    let conversation_id = body["conversation_id"].as_str()
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    tokio::spawn(crate::service_agent::run_interactive_agent(
        state, session_id.clone(), input, messages, system, use_tools,
        activity_context, vault_id, account_id, vault_path, conversation_id.clone(),
    ));

    Ok(Json(json!({ "session_id": session_id, "conversation_id": conversation_id })))
}

/// POST /vaults/:vid/agent/cancel
/// Body: { session_id }
pub async fn cancel(
    State(state): State<ApiState>,
    Path(_vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let session_id = body["session_id"].as_str().unwrap_or("");
    let (cancel_flag, tx_opt) = {
        let sessions = state.daemon.agent_sessions.lock().await;
        if let Some(sess) = sessions.values().find(|s| s.session_id == session_id) {
            (Some(Arc::clone(&sess.cancel)), sess.transaction.clone())
        } else {
            (None, None)
        }
    };
    if let Some(flag) = cancel_flag {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(tx) = tx_opt {
        let _ = tx.cancel().await;
    }
    Ok(Json(json!({ "ok": true })))
}

/// POST /vaults/:vid/agent/confirm
/// Body: { session_id, approved: bool }
pub async fn confirm(
    State(state): State<ApiState>,
    Path(_vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let session_id = body["session_id"].as_str().unwrap_or("");
    let approved = body["approved"].as_bool().unwrap_or(false);
    let tx_opt = {
        let sessions = state.daemon.agent_sessions.lock().await;
        sessions.values()
            .find(|s| s.session_id == session_id)
            .and_then(|s| s.transaction.clone())
    };
    if let Some(tx) = tx_opt {
        if approved {
            let _ = tx.commit().await;
        } else {
            let _ = tx.cancel().await;
        }
    }
    Ok(Json(json!({ "ok": true })))
}

/// POST /vaults/:vid/agent/live_chat
/// Body: { session_id, input, language, note_context, activity_context, vault_path, conversation_id }
/// Awaits run_live_chat_agent and returns { session_id, speech }.
pub async fn live_chat(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = account_id_from_headers(&state, &headers).await?;
    let session_id = body["session_id"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let input          = body["input"].as_str().unwrap_or("").to_string();
    let language       = body["language"].as_str().map(String::from);
    let note_context   = body["note_context"].as_str().map(String::from);
    let activity_context = body["activity_context"].as_str().map(String::from);
    let vault_path     = body["vault_path"].as_str().unwrap_or("").to_string();
    let conversation_id = body["conversation_id"].as_str().unwrap_or("").to_string();

    let speech = crate::service_agent::run_live_chat_agent(
        state, session_id.clone(), input, language, note_context,
        activity_context, vault_id, account_id, vault_path, conversation_id,
    ).await;

    Ok(Json(json!({ "session_id": session_id, "speech": speech })))
}
