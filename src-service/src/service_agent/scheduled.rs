use serde_json::{json, Value};
use crate::api_state::ApiState;
use crate::db::SurrealDb;

// ── Entry point ───────────────────────────────────────────────────────────────

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
            // No agent — just emit notification
            state.daemon.emit("schedule:triggered", json!({
                "task_id": task_id,
                "vault_id": vault_id,
                "description": description,
            }));
            return;
        }
    };

    let llm_url = state.daemon.llm_url.read().await.clone();
    let Some(llm_url) = llm_url else {
        tracing::warn!("[scheduler] llm_url not available, skipping task {}", task_id);
        return;
    };

    // Look up agent definition
    let agent_def = match super::helpers::load_agent_def(&state.db, &agent_name, &account_id).await {
        Some(a) => a,
        None => {
            tracing::warn!("[scheduler] agent '{}' not found for account '{}'", agent_name, account_id);
            return;
        }
    };

    let system_prompt = agent_def["system_prompt"].as_str().unwrap_or("").to_string();
    let max_rounds = agent_def["max_rounds"].as_i64().unwrap_or(super::MAX_ROUNDS as i64) as usize;
    let tool_names: Vec<String> = agent_def["tool_names"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let initial_msg = agent_prompt.unwrap_or_else(|| description.clone());

    tracing::info!(
        "[scheduler] running agent '{}' for task {} (tools: {:?})",
        agent_name, task_id, tool_names
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap_or_default();

    let embedding_url = state.daemon.embedding_url.read().await.clone();

    let result = run_agent_loop(
        &client,
        &llm_url,
        &state.db,
        &vault_id,
        &account_id,
        &embedding_url,
        &system_prompt,
        &initial_msg,
        &tool_names,
        max_rounds,
    ).await;

    state.daemon.emit("schedule:completed", json!({
        "task_id": task_id,
        "agent": agent_name,
        "vault_id": vault_id,
        "description": description,
        "summary": result,
    }));
}

// ── Agent loop ────────────────────────────────────────────────────────────────

async fn run_agent_loop(
    client: &reqwest::Client,
    llm_url: &str,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    embedding_url: &Option<String>,
    system_prompt: &str,
    initial_msg: &str,
    tool_names: &[String],
    max_rounds: usize,
) -> String {
    let tools_schema = super::memory_tools::build_scheduled_tools_schema(tool_names);
    let mut messages: Vec<Value> = vec![
        json!({ "role": "system", "content": system_prompt }),
        json!({ "role": "user",   "content": initial_msg }),
    ];

    let mut last_text = String::new();

    for _round in 0..max_rounds {
        let body = if tools_schema.is_empty() {
            json!({
                "messages": messages,
                "stream": false,
                "temperature": 0.3,
                "max_tokens": 2048,
            })
        } else {
            json!({
                "messages": messages,
                "tools": tools_schema,
                "tool_choice": "auto",
                "stream": false,
                "temperature": 0.3,
                "max_tokens": 2048,
            })
        };

        let Ok(resp) = client
            .post(format!("{}/v1/chat/completions", llm_url))
            .json(&body)
            .send().await
        else { break };

        let Ok(resp_json) = resp.json::<Value>().await else { break };

        let choice = &resp_json["choices"][0];
        let finish_reason = choice["finish_reason"].as_str().unwrap_or("stop");
        let message = &choice["message"];
        let text = message["content"].as_str().unwrap_or("").to_string();

        if !text.is_empty() {
            last_text = text.clone();
        }

        // Check for tool calls
        if finish_reason == "tool_calls" || message["tool_calls"].is_array() {
            let tool_calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
            if tool_calls.is_empty() { break; }

            // Add assistant message with tool_calls
            messages.push(message.clone());

            // Execute each tool call
            for tc in &tool_calls {
                let tc_id = tc["id"].as_str().unwrap_or("").to_string();
                let fn_name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                let fn_args: Value = tc["function"]["arguments"]
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(json!({}));

                let result = super::memory_tools::dispatch_scheduled_tool(client, llm_url, db, vault_id, account_id, embedding_url, &fn_name, &fn_args).await;
                let result_str = match &result {
                    Ok(v) => serde_json::to_string(v).unwrap_or_default(),
                    Err(e) => format!("ERROR: {}", e),
                };

                tracing::debug!("[scheduler] tool {} → {}", fn_name, &result_str[..result_str.len().min(200)]);

                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tc_id,
                    "content": result_str,
                }));
            }
        } else {
            // No tool calls — done
            break;
        }
    }

    last_text
}
