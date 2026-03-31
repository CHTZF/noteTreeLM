use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::json;

use crate::api_state::ApiState;

/// Execute a sub-agent that shares the parent conversation context.
///
/// - Uses non-streaming LLM calls (streaming: false) so tokens don't mix with parent's llm:token stream.
/// - Emits sub_agent:start / sub_agent:done / sub_agent:error.
/// - conversation_id is derived from parent so the sub-agent's messages are grouped together.
pub(crate) async fn run_sub_agent(
    state: &ApiState,
    vault_id: &str,
    account_id: &str,
    vault_path: &str,
    parent_session_id: &str,
    agent_name: &str,
    agent_def: serde_json::Value,
    input: &str,
    _parent_cancel: Arc<AtomicBool>,
) -> String {
    state.daemon.emit("sub_agent:start", json!({
        "parent_session_id": parent_session_id,
        "agent_name": agent_name,
        "input": input,
    }));

    // Derive a stable conversation_id so the sub-agent's history is persisted separately
    let conversation_id = format!("sub_{}_{}_{}", parent_session_id, agent_name, vault_id);

    let result = super::interactive::run_agent(
        state.clone(),
        agent_def,
        input.to_string(),
        vault_id.to_string(),
        account_id.to_string(),
        vault_path.to_string(),
        conversation_id,
        false, // silent — parent is already streaming
        None,
    ).await;

    state.daemon.emit("sub_agent:done", json!({
        "parent_session_id": parent_session_id,
        "agent_name": agent_name,
    }));

    result
}
