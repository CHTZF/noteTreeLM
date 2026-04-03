use serde_json::json;
use crate::api_state::ApiState;

pub async fn execute_scheduled_task(
    state: ApiState,
    task_id: String,
    vault_id: String,
    account_id: String,
    agent_def_name: Option<String>,
    agent_prompt: Option<String>,
    description: String,
) {
    let agent_name = match agent_def_name {
        Some(ref n) if !n.is_empty() => n.clone(),
        _ => {
            state.daemon.emit("schedule:triggered", json!({
                "task_id": task_id,
                "vault_id": vault_id,
                "description": description,
            }));
            return;
        }
    };

    // If account_id is empty (old task row without account_id), resolve from vault.
    let account_id = if account_id.is_empty() {
        #[derive(serde::Deserialize)]
        struct AccRow { account_id: String }
        state.db
            .query("SELECT account_id FROM vaults WHERE vault_id = $vid LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<AccRow>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| r.account_id)
            .unwrap_or_default()
    } else {
        account_id
    };

    if state.daemon.llm_url.read().await.is_none() {
        tracing::warn!("[scheduler] llm_url not available, skipping task {}", task_id);
        return;
    }

    let agent_def = match super::super::helpers::load_agent_def(&state.db, &agent_name, &account_id).await {
        Some(a) => a,
        None => {
            tracing::warn!("[scheduler] agent '{}' not found for account '{}'", agent_name, account_id);
            return;
        }
    };

    let initial_msg = agent_prompt.unwrap_or_else(|| description.clone());
    let conversation_id = format!("scheduled_{}_{}", task_id, agent_name);

    tracing::info!(
        "[scheduler] running agent '{}' for task {} vault_id='{}'",
        agent_name, task_id, vault_id
    );

    let result = super::interactive::run_agent(
        state.clone(),
        agent_def,
        initial_msg,
        vault_id.clone(),
        account_id,
        conversation_id,
        false, // background — no llm:token SSE
        None,
    ).await;

    state.daemon.emit("schedule:completed", json!({
        "task_id": task_id,
        "agent": agent_name,
        "vault_id": vault_id,
        "description": description,
        "summary": result,
    }));
}
