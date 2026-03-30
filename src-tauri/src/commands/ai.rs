use crate::{error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, State};

// ─── Re-exports from sub-modules ──────────────────────────────────────────────
pub use super::server::{
    get_embedding, warmup_llama_server,
    stop_llama_server, get_llama_server_status, start_llama_server, restart_llama_server,
    warmup_embedding_server, get_embedding_server_status, check_embedding_endpoint,
    start_embedding_server, stop_embedding_server, restart_embedding_server,
};
pub(crate) use super::server::ensure_server_running;
pub use super::external_ai::process_with_llm;
pub use super::memory::{
    query_memory,
    rate_response, get_conversation_ratings,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}
// LLM engine — see runtime/llm_engine.rs
pub use crate::runtime::llm_engine::compute_centroid;

/// 取消正在進行的 Agent 串流
#[tauri::command]
pub async fn cancel_agent(state: State<'_, AppState>) -> Result<(), AppError> {
    let session_id = state.agent_session.lock().await.clone();
    let vault_id = state.get_vault_id().await.unwrap_or_default();
    let auth_token = state.get_auth_token().await;
    let tok = if auth_token.is_empty() { None } else { Some(auth_token.as_str()) };

    if let Some(sid) = session_id {
        let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
            &state.http_client,
            &format!("/vaults/{}/agent/cancel", urlencoding::encode(&vault_id)),
            &serde_json::json!({ "session_id": sid }),
            tok,
        ).await;
    }
    // Keep local flag for the no-vault (live chat) path
    state.agent_cancel.store(true, Ordering::Relaxed);
    if let Some(tx) = state.write_confirm_tx.lock().await.take() {
        let _ = tx.send(false);
    }
    Ok(())
}

/// 前端確認/拒絕寫入工具
#[tauri::command]
pub async fn confirm_write_tool(
    state: State<'_, AppState>,
    approved: bool,
) -> Result<(), AppError> {
    let session_id = state.agent_session.lock().await.clone();
    let vault_id = state.get_vault_id().await.unwrap_or_default();
    let auth_token = state.get_auth_token().await;
    let tok = if auth_token.is_empty() { None } else { Some(auth_token.as_str()) };

    if let Some(sid) = session_id {
        let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
            &state.http_client,
            &format!("/vaults/{}/agent/confirm", urlencoding::encode(&vault_id)),
            &serde_json::json!({ "session_id": sid, "approved": approved }),
            tok,
        ).await;
    }
    // Keep local fallback for live chat path
    let tx = state.write_confirm_tx.lock().await.take();
    if let Some(tx) = tx {
        let _ = tx.send(approved);
    }
    Ok(())
}

/// 帶意圖分類的 Agent 串流（thin wrapper）
///
/// 所有前處理（pending plan、skill pre-pass、sliding window、DB 存取）
/// 全部委託給 notetreelm-service 的 run_interactive_agent 執行。
/// Tauri 僅負責：確保 llama-server 啟動、取得 vault 資訊、session 追蹤、POST 委派。
#[tauri::command]
pub async fn invoke_agent(
    _app: AppHandle,
    state: State<'_, AppState>,
    input: String,
    messages: Vec<ChatMessage>,
    system: Option<String>,
    use_tools: Option<bool>,
    conversation_id: Option<String>,
    activity_context: Option<String>,
) -> Result<serde_json::Value, AppError> {
    // 1. 確保 llama-server 運行（service 需要 llm_url）
    ensure_server_running(state.inner(), &_app).await?;

    // 2. Vault 資訊
    let vault_path = state.get_vault_path().await;
    let vault_id_opt = state.get_vault_id().await.ok();
    let auth_token = state.get_auth_token().await;
    let tok = if auth_token.is_empty() { None } else { Some(auth_token.as_str()) };

    // 3. Session 追蹤（cancel_agent 讀取 agent_session 來取得 session_id）
    let session_id = uuid::Uuid::new_v4().to_string();
    *state.agent_session.lock().await = Some(session_id.clone());
    state.agent_cancel.store(false, Ordering::SeqCst);

    // 4. 序列化 messages（無 conversation_id 時的 fallback，service 直接使用）
    let messages_json: Vec<serde_json::Value> = messages.iter()
        .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
        .collect();

    // 5. 委派給 service（pending plan、skill pre-pass、tool loop、DB 存取全在 service 處理）
    let vault_id_str = vault_id_opt.as_deref().unwrap_or("");
    let resp = crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/agent/run", urlencoding::encode(vault_id_str)),
        &serde_json::json!({
            "session_id": session_id,
            "input": input,
            "messages": messages_json,
            "system": system,
            "use_tools": use_tools.unwrap_or(true),
            "activity_context": activity_context,
            "vault_path": vault_path,
            "conversation_id": conversation_id,
        }),
        tok,
    ).await.map_err(|e| AppError::AI(e.to_string()))?;

    // Tokens/events 透過 SSE passthrough 抵達前端；回傳 session_id + conversation_id
    Ok(serde_json::json!({
        "session_id": session_id,
        "conversation_id": resp["conversation_id"],
    }))
}

/// 語音對話 Agent 串流（Live Chat 專用）
///
/// Thin wrapper：確保 llama-server 啟動、取得 vault 資訊、session 追蹤、POST 委派。
/// 所有邏輯（system prompt 組裝、memory prefetch、sliding window、DB 讀寫、3-round loop）
/// 全部委託給 notetreelm-service 的 run_live_chat_agent 執行。
#[tauri::command]
pub async fn invoke_live_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    conversation_id: String,
    input: String,
    note_context: Option<String>,
    activity_context: Option<String>,
    language: Option<String>,
) -> Result<String, AppError> {
    // 1. 確保 llama-server 運行
    ensure_server_running(state.inner(), &app).await?;

    // 2. Vault 資訊
    let vault_path = state.get_vault_path().await;
    let vault_id_opt = state.get_vault_id().await.ok();
    let auth_token = state.get_auth_token().await;
    let tok = if auth_token.is_empty() { None } else { Some(auth_token.as_str()) };

    // 3. Session 追蹤
    let session_id = uuid::Uuid::new_v4().to_string();
    *state.agent_session.lock().await = Some(session_id.clone());
    state.agent_cancel.store(false, Ordering::SeqCst);

    // 4. 委派給 service
    let vault_id_str = vault_id_opt.as_deref().unwrap_or("");
    let resp = crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/agent/live_chat", urlencoding::encode(vault_id_str)),
        &serde_json::json!({
            "session_id": session_id,
            "input": input,
            "language": language,
            "note_context": note_context,
            "activity_context": activity_context,
            "vault_path": vault_path,
            "conversation_id": conversation_id,
        }),
        tok,
    ).await.map_err(|e| AppError::AI(e.to_string()))?;

    let final_speech = resp["speech"].as_str().unwrap_or("").to_string();
    Ok(final_speech)
}


// tool_list_recent_conversations — see runtime/tool_dispatch.rs
pub use crate::runtime::tool_dispatch::tool_list_recent_conversations;

// Tool schemas — see runtime/tool_schema.rs
pub use crate::runtime::tool_schema::vault_tools;

// Tool dispatch — see runtime/tool_dispatch.rs
pub(crate) use crate::runtime::tool_dispatch::{
    resolve_vault_path, tool_list_structure, tool_read_note,
    tool_create_note, tool_update_note, tool_create_folder,
};


// FTS helpers — see runtime/fts_helpers.rs
#[allow(unused_imports)]
pub(crate) use crate::runtime::fts_helpers::{
    clean_fts_query, Comparison, parse_comparison, filter_lines_by_comparison,
};

// set_note_status — see commands/vault.rs
pub use crate::commands::vault::set_note_status;

// Pipeline commands — see commands/pipeline.rs
pub use crate::commands::pipeline::{
    cancel_tool_test, run_tool_pipeline, test_vault_tool,
};
// add_skill_trigger — see commands/agent_def.rs
pub use crate::commands::agent_def::add_skill_trigger;

