use std::sync::Arc;
use std::sync::atomic::Ordering;
use serde_json::{json, Value};
use super::super::harness::runtime::{HarnessRequestRuntime, AgentContextResult};
use super::super::types::AgentSession;

use super::super::harness::engine::planner::Planner;
use super::super::harness::engine::transaction::Transaction;
use super::super::harness::tool_def::build_tools_schema;

/// Run an agent defined by `runtime.agent_def`.
///
/// SSE events emitted:
///   llm:token              → plain string token
///   agent:tool_call        → {session_id, display}
///   agent:write_request    → {session_id, display}
///   agent:note_refs        → {session_id, paths: []}
///   agent:skills_activated → {session_id, titles: []}
///   agent:open_note        → Value::Array of paths
///   agent:cancelled        → null
///   agent:skill_suggestion → {query, response_preview}
///   agent:plan_announce    → {session_id, plan}
///   llm:done               → plain string (full response)
///
/// - `runtime.streaming: false` → silent background execution (scheduled tasks, sub-agents)
/// - `runtime.streaming: true`  → interactive execution with SSE (user-triggered named agent)
pub async fn run_agent(
    runtime: HarnessRequestRuntime,
    input: String,
    activity_context: Option<String>,
) -> String {
    // ── Step 0: Resume / intercept existing session ───────────────────────────
    if runtime.try_resume(&input).await {
        return String::new();
    }

    // ── Step 1: Extract agent params ─────────────────────────────────────────
    let enable_think = runtime.agent_def["enable_think"].as_bool().unwrap_or(false);
    let max_rounds   = runtime.agent_def["max_rounds"].as_u64().unwrap_or(super::super::MAX_ROUNDS as u64) as usize;

    // ── Step 2: Resolve tool list ─────────────────────────────────────────────
    let mut tool_names: Vec<String> = runtime.agent_def["tool_names"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    tool_names.retain(|t| t != "think");
    tool_names = inject_required_tools(tool_names, runtime.needs_frontend());

    // ── Step 3: Session registration ─────────────────────────────────────────
    let tx = Arc::new(Transaction::new());
    {
        let mut sessions = runtime.agent_sessions.lock().await;
        if let Some(old) = sessions.get(runtime.conv_id.as_str()) {
            old.cancel.store(true, Ordering::Relaxed);
            if let Some(ref old_tx) = old.transaction {
                let _ = old_tx.cancel().await;
            }
        }
        sessions.insert((*runtime.conv_id).clone(), AgentSession {
            session_id:     Arc::clone(&runtime.session_id),
            conv_id:        Arc::clone(&runtime.conv_id),
            cancel:         Arc::clone(&runtime.cancel),
            answer_channel: Arc::clone(&runtime.answer_channel),
            transaction:    Some(Arc::clone(&tx)),
        });
    }

    // ── Step 4: Emitter is pre-built in runtime ───────────────────────────────
    let emitter = &runtime.emitter;

    // ── Step 5+6: Pre-pass + context ─────────────────────────────────────────
    let AgentContextResult { mem_facts_count, activated_skill_titles, tool_names } =
        runtime.build_agent_context(&input, activity_context.as_deref(), tool_names).await;
    emitter.record_skill_activations(&activated_skill_titles);

    // ── Step 7: Tool loop ─────────────────────────────────────────────────────
    let full_response = run_tool_loop(
        &runtime, &tool_names,
        enable_think, max_rounds,
        Arc::clone(&tx),
    ).await;

    // ── Step 8: Post-processing ───────────────────────────────────────────────
    let msgs_final = runtime.get_context_msgs(false).await;
    runtime.post_process(&msgs_final, &full_response, &input, mem_facts_count).await;

    if runtime.is_background() {
        runtime.cleanup_session().await;
    }

    full_response
}

/// Vault tools whose presence indicates this agent operates on the vault.
const VAULT_TOOL_MARKERS: &[&str] = &[
    "read_note", "search_vault", "list_structure",
    "create_note", "update_note", "append_to_note", "delete_note",
    "read_then_write", "update_note_frontmatter",
];

fn inject_required_tools(mut tool_names: Vec<String>, needs_frontend: bool) -> Vec<String> {
    if tool_names.is_empty() { return tool_names; }

    for name in [
        "get_session_state",
        "compress_context", "finish",
        "checkpoint", "clear_checkpoint",
        "save_agent_knowledge", "get_agent_knowledge",
        "batch_apply",
    ] {
        if !tool_names.contains(&name.to_string()) {
            tool_names.push(name.to_string());
        }
    }

    if needs_frontend {
        for name in ["ask_user", "progress"] {
            if !tool_names.contains(&name.to_string()) {
                tool_names.push(name.to_string());
            }
        }
    }

    let has_vault_tools = tool_names.iter().any(|t| VAULT_TOOL_MARKERS.contains(&t.as_str()));
    if has_vault_tools && !tool_names.contains(&"get_vault_changes".to_string()) {
        tool_names.push("get_vault_changes".to_string());
    }

    tool_names
}

/// Single LLM invocation — handles both streaming and non-streaming, with or without tools.
/// Returns `(full_text, tool_chunks)` where each chunk is `(id, name, args_json_string)`.
async fn run_one_llm_round(
    runtime:     &HarnessRequestRuntime,
    msgs:        &[Value],
    tools:       Option<Value>,
    tool_choice: Value,
    emitter:     &super::super::harness::observability::emitter::ObservabilityEmitter,
) -> Result<(String, Vec<(String, String, String)>), String> {
    use super::super::harness::tools::llm;
    let has_tools = tools.is_some();
    if runtime.streaming {
        let body = match tools {
            Some(tv) => json!({ "messages": msgs, "tools": tv, "tool_choice": tool_choice,
                               "stream": true, "temperature": 0.7, "max_tokens": 2048 }),
            None     => json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 2048 }),
        };
        let wm = if has_tools { Some(&runtime.working_memory) } else { None };
        llm::stream_llm_round(
            &runtime.client, &runtime.llm_url, body, emitter, &runtime.cancel, wm,
        ).await.map(|(t, _, chunks)| (t, chunks))
    } else {
        llm::call_llm_once(
            &runtime.client, &runtime.llm_url, msgs, tools, &runtime.cancel,
        ).await
    }
}

async fn run_tool_loop(
    runtime:      &HarnessRequestRuntime,
    tool_names:   &[String],
    enable_think: bool,
    max_rounds:   usize,
    tx:           Arc<Transaction>,
) -> String {
    let emitter = &runtime.emitter;
    let mut full_response = String::new();

    if !tool_names.is_empty() {
        let tools_schema = build_tools_schema(tool_names);
        let tools_value  = if tools_schema.is_empty() { None } else { Some(json!(tools_schema)) };
        let think_schema = if enable_think {
            Some(build_tools_schema(&["think".to_string()]))
        } else {
            None
        };

        for round in 0..max_rounds.min(super::super::MAX_ROUNDS) {
            if runtime.cancel.load(Ordering::Relaxed) { break; }
            emitter.increment_round();

            let (round_tools, round_choice) = if round == 0 {
                if let Some(ref ts) = think_schema {
                    (Some(json!(ts)), json!({ "type": "function", "function": { "name": "think" } }))
                } else {
                    (tools_value.clone(), json!("auto"))
                }
            } else {
                (tools_value.clone(), json!("auto"))
            };

            let msgs_snapshot = runtime.get_context_msgs(true).await;

            let llm_t0 = std::time::Instant::now();
            let (text, tool_chunks) = match run_one_llm_round(
                runtime, &msgs_snapshot, round_tools, round_choice, emitter,
            ).await {
                Ok(r) => r,
                Err(e) => { tracing::warn!("[agent/tools] llm error: {}", e); break; }
            };
            emitter.record_llm_latency(llm_t0.elapsed().as_millis() as u64);

            if !text.is_empty() { full_response = text; }

            if !tool_chunks.is_empty() {
                let tc_json: Vec<Value> = tool_chunks.iter().map(|tc| json!({
                    "id": tc.0, "type": "function",
                    "function": { "name": tc.1, "arguments": tc.2 },
                })).collect();
                runtime.push_msg(json!({ "role": "assistant", "content": null, "tool_calls": tc_json })).await;

                let graph = Planner::plan_from_chunks(&tool_chunks);
                // During dispatch handlers may mutate msgs_buf (e.g. compress_context, finish).
                let results = match runtime.dispatch(Arc::clone(&tx), graph).await {
                    Ok(r) => r,
                    Err(e) => { tracing::warn!("[agent/tools] dispatcher error: {}", e); break; }
                };

                // Check finish signal before extending (handler may have set it).
                if let Some(answer) = runtime.take_finish_answer().await {
                    if runtime.streaming {
                        runtime.emit("llm:token", json!(answer));
                    }
                    full_response = answer;
                    break;
                }

                runtime.extend_msgs_guarded(Planner::results_to_messages(&tool_chunks, results)).await;

                if runtime.cancel.load(Ordering::Relaxed) { break; }
            } else {
                break;
            }
        }
    } else {
        emitter.increment_round();
        let msgs_snapshot = runtime.get_context_msgs(false).await;
        let llm_t0 = std::time::Instant::now();
        match run_one_llm_round(runtime, &msgs_snapshot, None, json!("auto"), emitter).await {
            Ok((text, _)) => { full_response = text; }
            Err(e) => { tracing::warn!("[agent/pure] llm error: {}", e); }
        }
        emitter.record_llm_latency(llm_t0.elapsed().as_millis() as u64);
    }

    full_response
}
