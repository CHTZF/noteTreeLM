use std::{sync::Arc, time::Duration};

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::app_state::ApiState;
use crate::service::harness::engine::transaction::Transaction;
use super::account_id_from_headers;

/// POST /vaults/:vid/agent/run
/// Body: { session_id, input, messages, system, activity_context, vault_path, conversation_id }
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
    let now = chrono::Utc::now().timestamp();
    let activity_context = body["activity_context"].as_str().map(String::from);
    let ui_language = body["ui_language"].as_str().map(String::from);
    let conversation_id = body["conversation_id"].as_str()
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let source_type = body["source_type"].as_str().map(String::from);
    let source_id   = body["source_id"].as_str().map(String::from);

    let agent_name = body["agent"].as_str().unwrap_or("chat");
    let agent_def = crate::service::helpers::load_agent_def(&state.db, agent_name, &account_id)
        .await
        .unwrap_or_else(|| json!({}));

    // Upsert conversation + seed messages before spawning.
    if !messages.is_empty() {
        let msgs_str = serde_json::to_string(&messages).unwrap_or_else(|_| "[]".to_string());
        let upsert = state.db
            .query("INSERT INTO conversations (id, account_id, vault_id, mode, title, messages_json, created_at, updated_at) \
                    VALUES (type::thing(\"conversations\", $cid), $aid, $vid, 'chat', '', $msgs, $now, $now) \
                    ON DUPLICATE KEY UPDATE messages_json = $msgs, updated_at = $now")
            .bind(("cid", conversation_id.clone()))
            .bind(("aid", account_id.clone()))
            .bind(("vid", vault_id.clone()))
            .bind(("msgs", msgs_str))
            .bind(("now", now))
            .await;
        if let Err(e) = upsert {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("failed to save conversation: {}", e)));
        }
    }

    let runtime = match crate::service::build_agent_runtime(&state,
        &vault_id, &account_id,
        Some(session_id.clone()),
        conversation_id.clone(),
        agent_def,
        true, // streaming
        ui_language.as_deref(),
        source_type,
        source_id,
    ).await {
        Some(r) => r,
        None => {
            state.daemon.emit("llm:done", serde_json::json!(""));
            return Ok(Json(json!({ "session_id": session_id, "conversation_id": conversation_id })));
        }
    };
    crate::service::run_agent(
        runtime,
        input,
        activity_context,
    ).await;

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
        if let Some(sess) = sessions.values().find(|s| s.session_id.as_str() == session_id) {
            (Some(Arc::clone(&sess.cancel)), sess.transaction.clone())
        } else {
            (None, None)
        }
    };
    if let Some(flag) = cancel_flag {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(tx) = tx_opt.map(|t: Arc<Transaction>| t) {
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
    let tx_opt: Option<Arc<Transaction>> = {
        let sessions = state.daemon.agent_sessions.lock().await;
        sessions.values()
            .find(|s| s.session_id.as_str() == session_id)
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
/// Runs run_agent (streaming:false) with skill pre-pass, then makes one final live_respond call.
pub async fn live_chat(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id      = account_id_from_headers(&state, &headers).await?;
    let session_id      = body["session_id"].as_str().map(String::from)
                            .unwrap_or_else(|| Uuid::new_v4().to_string());
    let input           = body["input"].as_str().unwrap_or("").to_string();
    let language        = body["language"].as_str().unwrap_or("zh-TW").to_string();
    let ui_language     = body["ui_language"].as_str().map(String::from);
    let note_context    = body["note_context"].as_str().map(String::from);
    let activity_context = body["activity_context"].as_str().map(String::from);
    let conversation_id = body["conversation_id"].as_str().unwrap_or("").to_string();

    // Load live_chat agent_def; fall back to empty def (skill pre-pass will fill tool_names).
    let agent_def = crate::service::helpers::load_agent_def(&state.db, "live_chat", &account_id)
        .await
        .unwrap_or_else(|| json!({
            "system_prompt": "",
            "tool_names": ["think", "search_skills"],
        }));

    // Phase 1: run_agent (streaming:false) — tool execution (think + skill tools).
    // live_respond is intentionally excluded; it's called separately below.
    let runtime = match crate::service::build_agent_runtime(&state,
        &vault_id, &account_id,
        Some(session_id.clone()),
        conversation_id.clone(),
        agent_def,
        false, // non-streaming: no llm:done / skill_suggestion events
        ui_language.as_deref(),
        None, None,
    ).await {
        Some(r) => r,
        None => return Ok(Json(json!({ "error": "LLM not configured" }))),
    };
    let agent_response = crate::service::run_agent(
        runtime,
        input.clone(),
        activity_context,
    ).await;

    // Phase 2: one final live_respond call to format speech.
    let llm_url = state.daemon.llm_url.clone();

    let lang_hint = match language.as_str() {
        "en" => "Reply in English.",
        "ja" => "日本語で返答してください。",
        "de" => "Bitte auf Deutsch antworten.",
        "ko" => "한국어로 답변해 주세요.",
        _    => "請用繁體中文口語回答。",
    };
    let note_hint = note_context.as_deref()
        .map(|nc| format!("\n[當前開啟的筆記]\n{}", nc))
        .unwrap_or_default();

    let live_respond_schema = crate::service::harness::tool_def::build_tools_schema(
        &["live_respond".to_string()],
    );
    let client = reqwest::Client::new();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let respond_msgs = vec![
        json!({ "role": "system", "content": format!(
            "你是語音助理。根據助理的回覆，呼叫 live_respond 輸出口語化的最終回覆。{}{}", lang_hint, note_hint
        )}),
        json!({ "role": "user",      "content": input }),
        json!({ "role": "assistant", "content": agent_response }),
    ];
    let respond_body = json!({
        "messages": respond_msgs,
        "tools": live_respond_schema,
        "tool_choice": "required",
        "stream": false,
        "temperature": 0.7,
        "max_tokens": 512,
    });

    let speech = match crate::service::tools::llm::call_llm_once(
        &client, &llm_url, &respond_msgs, Some(respond_body["tools"].clone()), &cancel,
    ).await {
        Ok((_, tool_chunks)) => {
            if let Some((_, _, args_str)) = tool_chunks.iter().find(|(_, n, _)| n == "live_respond") {
                let args: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
                let speech = args["speech"].as_str().unwrap_or("").to_string();
                state.daemon.emit("live_chat:action", args);
                speech
            } else {
                state.daemon.emit("live_chat:action", json!({ "speech": agent_response, "action": "none" }));
                agent_response.clone()
            }
        }
        Err(_) => {
            state.daemon.emit("live_chat:action", json!({ "speech": agent_response, "action": "none" }));
            agent_response.clone()
        }
    };

    Ok(Json(json!({ "session_id": session_id, "speech": speech })))
}

/// POST /vaults/:vid/agent/invoke
/// 同步 LLM 呼叫，直接等待回應後回傳。
/// Body: { system, input, tools?, tool_choice?, max_tokens?, temperature? }
/// Response: { text, tool_calls? }
pub async fn invoke(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(_vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let _account_id = account_id_from_headers(&state, &headers).await?;

    let system      = body["system"].as_str().unwrap_or("You are a helpful assistant.").to_string();
    let input       = body["input"].as_str().unwrap_or("").to_string();
    let max_tokens  = body["max_tokens"].as_u64().unwrap_or(1024) as u32;
    let temperature = body["temperature"].as_f64().unwrap_or(0.3) as f32;
    let tools       = body.get("tools").cloned();
    let tool_choice = body["tool_choice"].as_str().unwrap_or("auto").to_string();

    // Ensure llama is running
    let base_url = crate::routes::llm::ensure_llama_running(&state)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e))?;

    let client = reqwest::Client::new();
    let mut req = json!({
        "messages": [
            { "role": "system", "content": system },
            { "role": "user",   "content": input  },
        ],
        "stream":      false,
        "max_tokens":  max_tokens,
        "temperature": temperature,
    });
    if let Some(t) = tools {
        req["tools"]       = t;
        req["tool_choice"] = json!(tool_choice);
    }

    let resp = client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&req)
        .timeout(Duration::from_secs(120))
        .send().await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("llama 請求失敗：{}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err((StatusCode::BAD_GATEWAY, format!("llama 回應錯誤 {}：{}", status, text)));
    }

    let json: Value = resp.json().await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let text = json["choices"][0]["message"]["content"]
        .as_str().unwrap_or("").trim().to_string();
    let tool_calls = json.pointer("/choices/0/message/tool_calls").cloned();

    let mut result = json!({ "text": text });
    if let Some(tc) = tool_calls {
        if !tc.is_null() {
            result["tool_calls"] = tc;
        }
    }
    Ok(Json(result))
}
