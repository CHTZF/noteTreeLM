use axum::{
    extract::{Query, State, WebSocketUpgrade},
    extract::ws::{Message, WebSocket},
    response::IntoResponse,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::app_state::ApiState;

#[derive(Deserialize)]
pub struct WsQuery {
    token: Option<String>,
}

pub fn router() -> axum::Router<ApiState> {
    axum::Router::new().route("/ws", axum::routing::get(ws_handler))
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ApiState>,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state, query.token))
}

async fn handle_ws(socket: WebSocket, state: ApiState, token: Option<String>) {
    // Auth once on connect
    let account_id = match auth_token(&state, token).await {
        Some(id) => id,
        None => {
            let (mut tx, _) = socket.split();
            let _ = tx.send(Message::Text(
                json!({"type":"error","message":"unauthorized"}).to_string()
            )).await;
            return;
        }
    };

    let (mut tx, mut rx) = socket.split();
    let mut shutdown_rx = state.daemon.ws_shutdown_tx.subscribe();

    // Current running session_id for cancel support
    let mut current_session_id: Option<String> = None;

    loop {
        let msg = tokio::select! {
            m = rx.next() => match m {
                Some(Ok(m)) => m,
                _ => break,
            },
            _ = shutdown_rx.recv() => {
                let _ = tx.send(Message::Close(None)).await;
                break;
            }
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };

        let body: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match body["type"].as_str() {
            Some("cancel") => {
                if let Some(sid) = &current_session_id {
                    cancel_session(&state, sid).await;
                }
            }

            Some("confirm") => {
                // Write-tool confirmation from client: {"type":"confirm","approved":true/false}
                if let Some(sid) = &current_session_id {
                    let approved = body["approved"].as_bool().unwrap_or(false);
                    confirm_session(&state, sid, approved).await;
                }
            }

            Some("run") => {
                let vault_id = body["vault_id"].as_str().unwrap_or("").to_string();
                let input = body["input"].as_str().unwrap_or("").to_string();
                let session_id = body["session_id"].as_str()
                    .map(String::from)
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                let conversation_id = body["conversation_id"].as_str()
                    .map(String::from)
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                let platform = body["platform"].as_str().map(String::from);
                let active_note = body["active_note"].as_str().map(String::from);
                let selection = body["selection"].as_str().map(String::from);
                let ui_language = body["ui_language"].as_str().map(String::from);
                let agent_name = body["agent"].as_str().unwrap_or("chat").to_string();

                current_session_id = Some(session_id.clone());

                // Subscribe BEFORE spawning so we don't miss early tokens
                let mut event_rx = state.daemon.event_tx.subscribe();

                // Spawn agent
                let state2 = state.clone();
                let account_id2 = account_id.clone();
                let vault_id2 = vault_id.clone();
                let session_id2 = session_id.clone();
                let conversation_id2 = conversation_id.clone();
                let input2 = input.clone();
                tokio::spawn(async move {
                    let agent_def = crate::service::helpers::load_agent_def(
                        &state2.db, &agent_name, &account_id2,
                    ).await.unwrap_or_else(|| json!({}));

                    match crate::service::build_agent_runtime(
                        &state2, &vault_id2, &account_id2,
                        Some(session_id2),
                        conversation_id2,
                        agent_def,
                        true,
                        ui_language.as_deref(),
                        None, None,
                    ).await {
                        Some(mut runtime) => {
                            runtime.platform    = platform;
                            runtime.active_note = active_note;
                            runtime.selection   = selection;
                            crate::service::run_agent(runtime, input2, None).await;
                        }
                        None => {
                            state2.daemon.emit("llm:error", json!("LLM 未設定或無法啟動，請確認設定"));
                        }
                    }
                });

                // Forward events to client until llm:done / error / cancelled.
                // Timeout: 180s — covers slow LLM startup + long inference.
                let timeout = tokio::time::sleep(Duration::from_secs(180));
                tokio::pin!(timeout);

                'forward: loop {
                    tokio::select! {
                        result = event_rx.recv() => {
                            match result {
                                Ok(ev) => {
                                    // Build a unified envelope: {"event":"<name>","data":<payload>}
                                    // Special cases add extra fields for convenience.
                                    let out = match ev.event.as_str() {
                                        "llm:done" => {
                                            // payload is now { "t": string, "session_id": string }
                                            // Extract text so WebSocket clients get a plain string.
                                            let data = match &ev.payload {
                                                Value::String(s) => s.clone(),
                                                Value::Object(m) => m.get("t")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                v => v.to_string(),
                                            };
                                            json!({
                                                "event": "llm:done",
                                                "data": data,
                                                "conversation_id": conversation_id,
                                            }).to_string()
                                        }
                                        "llm:token" => {
                                            // payload is now { "t": string, "session_id": string }
                                            // Extract text so WebSocket clients get a plain string.
                                            let data = match &ev.payload {
                                                Value::String(s) => s.clone(),
                                                Value::Object(m) => m.get("t")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                _ => String::new(),
                                            };
                                            json!({
                                                "event": "llm:token",
                                                "data": data,
                                            }).to_string()
                                        }
                                        // Forward all other events as-is
                                        _ => json!({
                                            "event": ev.event,
                                            "data": ev.payload,
                                        }).to_string(),
                                    };

                                    let done = matches!(
                                        ev.event.as_str(),
                                        "llm:done" | "llm:error" | "agent:cancelled"
                                    );
                                    if tx.send(Message::Text(out)).await.is_err() {
                                        return; // client disconnected
                                    }
                                    if done {
                                        current_session_id = None;
                                        break 'forward;
                                    }
                                }
                                Err(_) => break 'forward,
                            }
                        }
                        _ = &mut timeout => {
                            let _ = tx.send(Message::Text(
                                json!({"event":"llm:error","data":"LLM 回應超時，請確認 LLM 伺服器是否正常運行"}).to_string()
                            )).await;
                            current_session_id = None;
                            break 'forward;
                        }
                    }
                }
            }

            _ => {}
        }
    }
}

async fn auth_token(state: &ApiState, token: Option<String>) -> Option<String> {
    let tok = token?;
    let now = Utc::now().timestamp();
    #[derive(serde::Deserialize)]
    struct Row { username: String }
    state.db
        .query("SELECT username FROM sessions WHERE token = $t AND expires_at > $now LIMIT 1")
        .bind(("t", tok))
        .bind(("now", now))
        .await
        .ok()?
        .take::<Vec<Row>>(0)
        .ok()?
        .into_iter()
        .next()
        .map(|r| r.username)
}

async fn confirm_session(state: &ApiState, session_id: &str, approved: bool) {
    let tx_opt = {
        let sessions = state.daemon.agent_sessions.lock().await;
        sessions.values()
            .find(|s| s.session_id.as_str() == session_id)
            .and_then(|s| s.transaction.clone())
    };
    if let Some(tx) = tx_opt.map(|t: Arc<crate::service::harness::engine::transaction::Transaction>| t) {
        if approved { let _ = tx.commit().await; }
        else        { let _ = tx.cancel().await; }
    }
}

async fn cancel_session(state: &ApiState, session_id: &str) {
    let (cancel_flag, tx_opt) = {
        let sessions = state.daemon.agent_sessions.lock().await;
        if let Some(sess) = sessions.values().find(|s| s.session_id.as_str() == session_id) {
            (
                Some(Arc::clone(&sess.cancel)),
                sess.transaction.clone(),
            )
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
}
