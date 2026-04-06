use serde_json::json;
use crate::service::harness::runtime::HarnessRequestRuntime;

pub async fn execute_scheduled_task(
    runtime: HarnessRequestRuntime,
    task_id: String,
    description: String,
    initial_msg: String,
) {
    let agent_name = runtime.agent_def["name"].as_str().unwrap_or("unknown").to_string();
    let vault_id   = runtime.vault_id.clone();

    tracing::info!(
        "[scheduler] running agent '{}' for task {} vault_id='{}'",
        agent_name, task_id, vault_id
    );

    let result = super::agent::run_agent(runtime.clone(), initial_msg, None).await;

    runtime.emit("schedule:completed", json!({
        "task_id":      task_id,
        "agent":        agent_name,
        "vault_id":     vault_id,
        "description":  description,
        "summary":      result,
    }));
}
