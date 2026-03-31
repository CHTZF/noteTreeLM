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

    // Resolve vault_path from DB
    let vault_path = {
        #[derive(serde::Deserialize)]
        struct Row { path: String }
        state.db
            .query("SELECT path FROM vaults WHERE vault_id = $vid LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| r.path)
            .unwrap_or_default()
    };

    let initial_msg = agent_prompt.unwrap_or_else(|| description.clone());
    let conversation_id = format!("scheduled_{}_{}", task_id, agent_name);

    tracing::info!(
        "[scheduler] running agent '{}' for task {} vault_path='{}'",
        agent_name, task_id, vault_path
    );

    let result = super::interactive::run_agent(
        state.clone(),
        agent_def,
        initial_msg,
        vault_id.clone(),
        account_id,
        vault_path,
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
