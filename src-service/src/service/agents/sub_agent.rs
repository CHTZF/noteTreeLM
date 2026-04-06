use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::json;

use crate::service::HarnessRequestRuntime;

/// Execute a sub-agent that shares the parent's infrastructure context.
///
/// - Forks a new `HarnessRequestRuntime` with a fresh session/working-memory
///   but the same db, llm_url, vault_id, account_id, and caches as the parent.
/// - Shares the parent's cancel flag so cancelling the parent also stops the sub-agent.
/// - Uses non-streaming LLM calls so tokens don't mix with parent's llm:token stream.
/// - Emits sub_agent:start / sub_agent:done / sub_agent:error.
pub(crate) async fn run_sub_agent(
    runtime: &HarnessRequestRuntime,
    parent_session_id: &str,
    agent_name: &str,
    agent_def: serde_json::Value,
    input: &str,
    parent_cancel: Arc<AtomicBool>,
) -> String {
    runtime.emit("sub_agent:start", json!({
        "parent_session_id": parent_session_id,
        "agent_name": agent_name,
        "input": input,
    }));

    let conversation_id = format!("sub_{}_{}_{}", parent_session_id, agent_name, runtime.vault_id);
    let mut sub_runtime = runtime.fork_for_sub_agent(conversation_id, agent_def);
    sub_runtime.cancel = Arc::clone(&parent_cancel);

    let result = super::agent::run_agent(
        sub_runtime,
        input.to_string(),
        None,
    ).await;

    if result.is_empty() {
        runtime.emit("sub_agent:error", json!({
            "parent_session_id": parent_session_id,
            "agent_name": agent_name,
            "reason": "no response from LLM",
        }));
    } else {
        runtime.emit("sub_agent:done", json!({
            "parent_session_id": parent_session_id,
            "agent_name": agent_name,
        }));
    }

    result
}
