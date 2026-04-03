use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use serde_json::{json, Value};
use crate::api_state::ApiState;
use crate::db::SurrealDb;
use crate::state::AgentSession;

use super::super::engine::dispatcher::Dispatcher;
use super::super::engine::planner::Planner;
use super::super::engine::tool_registry::ToolRegistry;
use super::super::engine::transaction::Transaction;
use super::super::engine::context::{build_messages, inject_memory, trim_context};
use super::super::types::{EmitEventFn, IsWriteFn, Tool};
use super::super::helpers::MetaFunctionSpec;

/// Public entry point called from routes/agents.rs for user-facing agent runs.
/// Now receives raw input + thin params from Tauri; does all pre-processing here.
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
pub(crate) async fn run_interactive_agent(
    state: ApiState,
    session_id: String,
    input: String,
    system: String,
    streaming: bool,               // true = emit llm:token SSE; false = silent (background/sub-agent)
    tool_names: Vec<String>,       // pre-resolved by caller (runner or run_agent)
    system_injection: String,      // extra text appended to system prompt (from skill pass)
    activity_context: Option<String>,
    vault_id: String,
    account_id: String,
    vault_path: String,
    conversation_id: String,
    // Memory facts pre-fetched by run_agent in parallel with skill_pass.
    // When Some, skip the in-body fetch (step 4b). When None, fetch here (legacy / direct callers).
    prefetched_memory: Option<(Vec<serde_json::Value>, Vec<String>)>,
    // Session cancel flag and transaction — created in run_agent and passed in.
    cancel: Arc<AtomicBool>,
    tx: Arc<Transaction>,
    // Pre-planner meta-functions generated from matched skills' tool_chain_order.
    meta_functions: Vec<MetaFunctionSpec>,
) -> String {
    let conv_id = conversation_id;

    // 2. Resolve llm_url
    let llm_url = match state.daemon.llm_url.read().await.clone() {
        Some(u) => u,
        None => {
            state.daemon.emit("llm:done", json!(""));
            return String::new();
        }
    };

    let mut tool_names = tool_names; // rebind as mutable for chain filtering below

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .unwrap_or_default();
    let embedding_url = state.daemon.embedding_url.read().await.clone();

    // 3. Build messages: load history + user input + system prompt assembly
    let mut messages_json = build_messages(
        &state.db, &conv_id, &input,
        &system, activity_context.as_deref(), &system_injection,
    ).await;

    // 4. Inject memory facts (pre-fetched or inline)
    let state_c = state.clone();
    inject_memory(
        &mut messages_json,
        &client, &embedding_url, &state.db,
        &vault_id, &account_id, &input,
        prefetched_memory,
        streaming,
        &move |fact_ids| {
            state_c.daemon.emit("memory:prefetched", json!({
                "node_ids": fact_ids,
                "source": "chat",
            }));
        },
    ).await;

    // 5. Trim context window (summarize oldest messages when over limit)
    trim_context(
        &mut messages_json,
        &client, &llm_url,
        !tool_names.is_empty(), !vault_path.is_empty(),
    ).await;

    // 6. Build ToolRegistry + Dispatcher
    let emit_fn_closure: EmitEventFn = {
        let state_c = state.clone();
        let session_id_c = session_id.clone();
        Arc::new(move |event: String, mut payload: Value| {
            // Enrich object payloads with session_id
            if let Value::Object(ref mut m) = payload {
                m.insert("session_id".to_string(), json!(session_id_c));
            }
            state_c.daemon.emit(&event, payload);
        })
    };

    let is_write_fn: IsWriteFn = Arc::new(|name: &str| {
        super::super::tools::vault_tools::is_interactive_write_tool(name)
    });

    // Remove chain tools covered by meta-functions from the direct tool list.
    let chain_tool_set: std::collections::HashSet<String> = meta_functions.iter()
        .flat_map(|m| m.chain.iter().cloned())
        .collect();
    tool_names.retain(|t| !chain_tool_set.contains(t));

    let mut registry = build_interactive_registry(
        &client, &llm_url, &state.db,
        &vault_id, &account_id, &vault_path, &embedding_url,
        &session_id, &state,
        Arc::clone(&tx),
        Arc::clone(&cancel),
    );

    // If meta_functions exist, LLM schema only contains them → tool loop always
    // expands via plan_from_meta_function. Determined once before the loop.
    let has_meta = !meta_functions.is_empty();

    let dispatcher = Dispatcher::new(
        Arc::new(registry),
        Arc::clone(&emit_fn_closure),
        Arc::clone(&is_write_fn),
    );

    // 7. Tool loop or no-tool LLM
    // Exclude "think" from the main loop schema — it's handled in the pre-think block only.
    let main_tool_names: Vec<String> = tool_names.iter()
        .filter(|t| t.as_str() != "think")
        .cloned()
        .collect();
    let mut tools_schema = super::super::tools::vault_tools::build_tools_schema_interactive(&main_tool_names);
    // Append meta-function schemas so LLM sees them as callable tools.
    for spec in &meta_functions {
        tools_schema.push(build_meta_fn_schema(spec));
    }
    let mut msgs = messages_json.clone();
    let mut full_response = String::new();

    if (!tool_names.is_empty() || !meta_functions.is_empty()) && !vault_path.is_empty() {
        let tools_value = if tools_schema.is_empty() {
            None
        } else {
            Some(json!(tools_schema))
        };

        // Pre-think: if think tool is available, call LLM once (non-streaming) to get a thought,
        // emit agent:think, then append the tool call + result to msgs before the main loop.
        if tool_names.contains(&"think".to_string()) && streaming {
            let think_schema = json!([{
                "type": "function",
                "function": {
                    "name": "think",
                    "description": "輸出一句內心獨白（10字以內），描述正在想什麼",
                    "parameters": { "type": "object", "properties": {
                        "thought": { "type": "string" }
                    }, "required": ["thought"] }
                }
            }]);
            let think_body = json!({
                "messages": msgs,
                "tools": think_schema,
                "tool_choice": { "type": "function", "function": { "name": "think" } },
                "stream": false,
                "temperature": 0.7,
                "max_tokens": 64,
            });
            if let Ok(resp) = client
                .post(format!("{}/v1/chat/completions", llm_url))
                .json(&think_body)
                .send().await
            {
                if let Ok(j) = resp.json::<Value>().await {
                    let msg = &j["choices"][0]["message"];
                    if let Some(tcs) = msg["tool_calls"].as_array() {
                        if let Some(tc) = tcs.first() {
                            let id = tc["id"].as_str().unwrap_or("think_0").to_string();
                            let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}").to_string();
                            let args: Value = serde_json::from_str(&args_str).unwrap_or_default();
                            let thought = args["thought"].as_str().unwrap_or("").to_string();
                            if !thought.is_empty() {
                                emit_fn_closure("agent:think".to_string(), json!({ "thought": thought }));
                                msgs.push(json!({
                                    "role": "assistant", "content": null,
                                    "tool_calls": [{ "id": id, "type": "function", "function": { "name": "think", "arguments": args_str } }]
                                }));
                                msgs.push(json!({ "role": "tool", "tool_call_id": id, "content": "✅" }));
                            }
                        }
                    }
                }
            }
        }

        for _round in 0..super::super::MAX_ROUNDS {
            if cancel.load(Ordering::Relaxed) { break; }

            let (text, tool_chunks) = if streaming {
                let body = if tools_value.is_none() {
                    json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 2048 })
                } else {
                    json!({
                        "messages": msgs,
                        "tools": tools_value,
                        "tool_choice": "auto",
                        "stream": true,
                        "temperature": 0.7,
                        "max_tokens": 2048,
                    })
                };
                match super::super::tools::vault_tools::stream_llm_round(
                    &client, &llm_url, body, &state, &session_id, &cancel,
                ).await {
                    Ok((t, _, chunks)) => (t, chunks),
                    Err(e) => { tracing::warn!("[interactive] stream error: {}", e); break; }
                }
            } else {
                match super::super::tools::vault_tools::call_llm_once(
                    &client, &llm_url, &msgs, tools_value.clone(), &cancel,
                ).await {
                    Ok(r) => r,
                    Err(e) => { tracing::warn!("[interactive] llm error: {}", e); break; }
                }
            };


            if !text.is_empty() {
                full_response = text.clone();
            }

            if !tool_chunks.is_empty() {
                let tc_json: Vec<Value> = tool_chunks.iter().map(|tc| json!({
                    "id": tc.0, "type": "function",
                    "function": { "name": tc.1, "arguments": tc.2 },
                })).collect();
                msgs.push(json!({ "role": "assistant", "content": null, "tool_calls": tc_json }));

                // meta_functions path: LLM called a skill meta-function → expand chain
                // into a flat ToolGraph. has_meta is fixed before the loop (pre-pass result).
                let (graph, tc_id, tc_name) = if has_meta {
                    let tc = &tool_chunks[0];
                    let spec = meta_functions.iter().find(|s| s.fn_name == tc.1);
                    if spec.is_none() {
                        // LLM called an unknown meta_function fn_name — enter discovery mode:
                        // return discovery context as tool result so LLM can guide the user.
                        let discovery = build_skill_discovery_injection(&state.db, &account_id).await;
                        msgs.push(json!({ "role": "tool", "tool_call_id": tc.0, "name": tc.1, "content": discovery }));
                        if cancel.load(Ordering::Relaxed) { break; }
                        continue;
                    }
                    let spec = spec.unwrap();
                    // Bump trigger_count for the skill LLM actually selected.
                    {
                        let db = state.db.clone();
                        let sid = spec.skill_id.clone();
                        let now = chrono::Utc::now().timestamp();
                        tokio::spawn(async move {
                            let _ = db
                                .query("UPDATE agent_skills SET trigger_count = (trigger_count OR 0) + 1, last_triggered_at = $now WHERE skill_id = $sid")
                                .bind(("now", now)).bind(("sid", sid)).await;
                        });
                    }
                    let user_args: Value = serde_json::from_str(&tc.2).unwrap_or(json!({}));
                    let g = Planner::plan_from_meta_function(&spec.chain, &user_args);
                    (g, Some(tc.0.clone()), Some(tc.1.clone()))
                } else {
                    (Planner::plan_from_chunks(&tool_chunks), None, None)
                };

                let results = match dispatcher.run(Arc::clone(&tx), graph).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("[interactive] dispatcher error: {}", e);
                        break;
                    }
                };

                // meta_function: map last chain result back to the original tool_call_id.
                let tool_messages = if let (Some(id), Some(name)) = (tc_id, tc_name) {
                    let content = results.into_iter().last()
                        .map(|v| match v {
                            Value::String(s) => s,
                            other => serde_json::to_string(&other).unwrap_or_default(),
                        })
                        .unwrap_or_default();
                    vec![json!({ "role": "tool", "tool_call_id": id, "name": name, "content": content })]
                } else {
                    Planner::results_to_messages(&tool_chunks, results)
                };
                msgs.extend(tool_messages);

                if cancel.load(Ordering::Relaxed) { break; }
            } else if has_meta && !tool_chunks.is_empty() {
                // tool_chunks non-empty but fell through (shouldn't happen) → break
                break;
            } else if has_meta {
                // has_meta but LLM returned text without calling any meta_function.
                // Inject discovery context as a system reminder and retry once.
                let discovery = build_skill_discovery_injection(&state.db, &account_id).await;
                msgs.push(json!({ "role": "system", "content": discovery }));
                if cancel.load(Ordering::Relaxed) { break; }
                // continue loop — LLM gets another chance with discovery context
            } else {
                break;
            }
        }
    } else {
        // No tools or no vault → pure LLM call, streaming or silent
        if streaming {
            let body = json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 2048 });
            match super::super::tools::vault_tools::stream_llm_round(&client, &llm_url, body, &state, &session_id, &cancel).await {
                Ok((text, _, _)) => { full_response = text; }
                Err(e) => { tracing::warn!("[interactive] no-vault stream error: {}", e); }
            }
        } else {
            match super::super::tools::vault_tools::call_llm_once(&client, &llm_url, &msgs, None, &cancel).await {
                Ok((text, _)) => { full_response = text; }
                Err(e) => { tracing::warn!("[interactive] no-vault llm error: {}", e); }
            }
        }
    }

    if streaming {
        state.daemon.emit("llm:done", json!(full_response));

        // 9. Skill suggestion detection (only relevant for interactive sessions)
        if !full_response.is_empty() && super::super::helpers::detect_response_framework(&full_response) {
            state.daemon.emit("agent:skill_suggestion", json!({
                "query": input,
                "response_preview": full_response.chars().take(200).collect::<String>(),
            }));
        }
    }

    // 10. Persist conversation to DB
    {
        let mut to_save: Vec<Value> = msgs.iter()
            .filter(|m| m["role"].as_str() != Some("system"))
            .cloned()
            .collect();
        if !full_response.is_empty() {
            to_save.push(json!({ "role": "assistant", "content": full_response }));
        }
        let now = chrono::Utc::now().timestamp();
        let _ = state.db
            .query("UPDATE conversations SET messages_json = $msgs, updated_at = $now WHERE record::id(id) = $cid")
            .bind(("msgs", serde_json::to_string(&to_save).unwrap_or_else(|_| "[]".to_string())))
            .bind(("now", now))
            .bind(("cid", conv_id.clone()))
            .await;
        maybe_set_conv_title(&state.db, &conv_id, &to_save).await;
    }

    full_response
}

/// Build a ToolRegistry where every closure calls `dispatch_interactive_tool`.
/// Special tools (plan_announce, open_note) emit their own SSE from within the closure.
pub(crate) fn build_interactive_registry(
    client: &reqwest::Client,
    llm_url: &str,
    db: &crate::db::SurrealDb,
    vault_id: &str,
    account_id: &str,
    vault_path: &str,
    embedding_url: &Option<String>,
    session_id: &str,
    state: &ApiState,
    _tx: Arc<Transaction>,
    cancel: Arc<AtomicBool>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    let all_tool_names = [
        "list_structure", "read_note", "create_note", "update_note", "create_folder",
        "search_vault", "query_memory", "delete_note", "delete_folder", "move_note",
        "append_to_note", "create_agent_skill", "plan_announce",
        "open_note", "get_unprocessed_conversations", "get_conversation_content",
        "save_memory_facts", "mark_conversation_processed", "condense_memory_facts",
    ];

    for &name in &all_tool_names {
        let client = client.clone();
        let llm_url = llm_url.to_string();
        let db = db.clone();
        let vault_id = vault_id.to_string();
        let account_id = account_id.to_string();
        let vault_path = vault_path.to_string();
        let embedding_url = embedding_url.clone();
        let session_id = session_id.to_string();
        let state_c = state.clone();
        let name_owned = name.to_string();

        let execute: super::super::types::ToolFn = Arc::new(move |args: Value| {
            let client = client.clone();
            let llm_url = llm_url.clone();
            let db = db.clone();
            let vault_id = vault_id.clone();
            let account_id = account_id.clone();
            let vault_path = vault_path.clone();
            let embedding_url = embedding_url.clone();
            let session_id = session_id.clone();
            let state_c = state_c.clone();
            let name = name_owned.clone();

            Box::pin(async move {
                // Special: plan_announce — emit SSE before returning
                if name == "plan_announce" {
                    let plan = args["plan"].as_str().unwrap_or("").to_string();
                    state_c.daemon.emit("agent:plan_announce", json!({
                        "session_id": session_id,
                        "plan": plan,
                    }));
                    return Ok(json!("✅ 已確認計畫，請立即執行"));
                }

                let result = super::super::tools::vault_tools::dispatch_interactive_tool(
                    &client, &llm_url, &db,
                    &vault_id, &account_id, &vault_path, &embedding_url,
                    &name, &args,
                ).await;

                match result {
                    Ok((v, _rollback)) => {
                        // Post-dispatch: emit note_refs
                        let refs = super::super::tools::vault_tools::extract_note_refs(&name, &args, &v, &vault_path);
                        if !refs.is_empty() {
                            state_c.daemon.emit("agent:note_refs", json!({
                                "session_id": session_id,
                                "paths": refs,
                            }));
                        }
                        // Special: search_skills — emit which skills were found
                        if name == "search_skills" {
                            if let Some(arr) = v.as_array() {
                                let titles: Vec<String> = arr.iter()
                                    .filter_map(|s| s["title"].as_str().map(String::from))
                                    .collect();
                                if !titles.is_empty() {
                                    state_c.daemon.emit("agent:skills_activated", json!({
                                        "session_id": session_id,
                                        "titles": titles,
                                        "source": "search_skills",
                                    }));
                                }
                            }
                        }
                        // Special: open_note — also emit agent:open_note
                        if name == "open_note" {
                            let paths: Vec<Value> = args["paths"].as_array()
                                .cloned()
                                .unwrap_or_else(|| {
                                    args["path"].as_str()
                                        .map(|p| vec![json!(p)])
                                        .unwrap_or_default()
                                });
                            state_c.daemon.emit("agent:open_note", json!(paths));
                        }
                        Ok(v)
                    }
                    Err(e) => Err(e),
                }
            })
        });

        registry.register(name.to_string(), Tool { execute, rollback: None });
    }

    // ── call_agent tool ──────────────────────────────────────────────────────────
    {
        let db         = db.clone();
        let vault_id   = vault_id.to_string();
        let account_id = account_id.to_string();
        let vault_path = vault_path.to_string();
        let session_id = session_id.to_string();
        let state_c    = state.clone();
        let cancel_c   = Arc::clone(&cancel);

        let execute: super::super::types::ToolFn = Arc::new(move |args: Value| {
            let db         = db.clone();
            let vault_id   = vault_id.clone();
            let account_id = account_id.clone();
            let vault_path = vault_path.clone();
            let session_id = session_id.clone();
            let state_c    = state_c.clone();
            let cancel_c   = Arc::clone(&cancel_c);

            Box::pin(async move {
                let agent_name = args["name"].as_str().unwrap_or("").to_string();
                let input      = args["input"].as_str().unwrap_or("").to_string();
                if agent_name.is_empty() {
                    return Err("call_agent: missing agent name".into());
                }

                let def = match super::super::helpers::load_agent_def(&db, &agent_name, &account_id).await {
                    Some(d) => d,
                    None => return Err(format!("call_agent: agent '{}' not found", agent_name)),
                };

                let result = super::sub_agent::run_sub_agent(
                    &state_c,
                    &vault_id, &account_id, &vault_path,
                    &session_id, &agent_name,
                    def, &input,
                    cancel_c,
                ).await;

                Ok(serde_json::json!(result))
            })
        });

        registry.register("call_agent".to_string(), Tool { execute, rollback: None });
    }

    registry
}

pub async fn cleanup_session(state: &ApiState, conv_id: &str) {
    let mut sessions = state.daemon.agent_sessions.lock().await;
    sessions.remove(conv_id);
}

/// Set conversation title from first user message if title is still empty / default.
async fn maybe_set_conv_title(db: &SurrealDb, conv_id: &str, messages: &[Value]) {
    #[derive(serde::Deserialize)]
    struct Row { title: String }
    let current_title = db
        .query("SELECT title FROM conversations WHERE record::id(id) = $cid LIMIT 1")
        .bind(("cid", conv_id.to_string()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .map(|r| r.title)
        .unwrap_or_default();

    if !current_title.is_empty() && current_title != "New Conversation" && current_title != "新對話" {
        return;
    }

    let auto_title = messages.iter()
        .find(|m| m["role"].as_str() == Some("user"))
        .and_then(|m| m["content"].as_str())
        .map(|c| {
            let chars: String = c.chars().take(20).collect();
            if c.chars().count() > 20 { format!("{}…", chars) } else { chars }
        });

    if let Some(title) = auto_title {
        let now = chrono::Utc::now().timestamp();
        let _ = db
            .query("UPDATE conversations SET title = $title, updated_at = $now WHERE record::id(id) = $cid")
            .bind(("title", title))
            .bind(("now", now))
            .bind(("cid", conv_id.to_string()))
            .await;
    }
}

// ── Unified agent runner ───────────────────────────────────────────────────────

/// Run an agent defined by `agent_def` (from DB or inline).
/// Thin wrapper around `run_interactive_agent` that extracts the spec and wires
/// `tool_names_override` so skill matching is bypassed.
///
/// - `streaming: false` → silent background execution (scheduled tasks, sub-agents)
/// - `streaming: true`  → interactive execution with SSE (user-triggered named agent)
pub async fn run_agent(
    state: ApiState,
    agent_def: Value,
    input: String,
    vault_id: String,
    account_id: String,
    vault_path: String,
    conversation_id: String,
    streaming: bool,
    activity_context: Option<String>,
) -> String {
    let conv_id = &conversation_id;

    // Derive session_id early (needed for session registration below).
    let session_id = agent_def["session_id"].as_str()
        .map(String::from)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // 0. Write-confirmation intercept: if the same conversation has a Preparing
    //    transaction, classify input intent and commit/cancel it instead of
    //    starting a new LLM round.
    {
        let pending_tx: Option<Arc<Transaction>> = {
            let sessions = state.daemon.agent_sessions.lock().await;
            sessions.get(conv_id).and_then(|s| s.transaction.clone())
        };
        if let Some(pending) = pending_tx {
            use super::super::engine::transaction::TransactionState;
            if pending.state().await == TransactionState::Preparing {
                let embed_fn: super::super::types::EmbedFn = {
                    let client = reqwest::Client::new();
                    let llm_url = state.daemon.llm_url.read().await.clone().unwrap_or_default();
                    Arc::new(move |text: String| {
                        let client = client.clone();
                        let llm_url = llm_url.clone();
                        Box::pin(async move {
                            super::super::helpers::embed_text_llm(&client, &llm_url, &text).await
                        })
                    })
                };
                let classifier = super::super::engine::intent_classifier::IntentClassifier::new();
                let intent = match super::super::engine::intent_classifier::IntentClassifier::compute_centroids_cached(
                    &state.daemon.intent_centroids,
                    &embed_fn,
                ).await {
                    Some((cc, ccl, ci)) => classifier.classify_with_centroids(&input, &embed_fn, &cc, &ccl, &ci).await,
                    None => classifier.classify(&input).await,
                };
                use super::super::engine::intent_classifier::Intent;
                match intent {
                    Intent::Confirm => { let _ = pending.commit().await; }
                    Intent::Cancel | Intent::Interrupt => { let _ = pending.cancel().await; }
                    _ => {}
                }
                if matches!(intent, Intent::Confirm | Intent::Cancel | Intent::Interrupt) {
                    return String::new();
                }
            }
        }
    }

    // 1. Register session (cancel flag + transaction).
    //    Cancel any existing session for this conversation first.
    let cancel = Arc::new(AtomicBool::new(false));
    let tx = Arc::new(Transaction::new());
    {
        let mut sessions = state.daemon.agent_sessions.lock().await;
        if let Some(old) = sessions.get(conv_id) {
            old.cancel.store(true, Ordering::Relaxed);
            if let Some(ref old_tx) = old.transaction {
                let _ = old_tx.cancel().await;
            }
        }
        sessions.insert(conv_id.to_string(), AgentSession {
            session_id: session_id.clone(),
            cancel: Arc::clone(&cancel),
            transaction: Some(Arc::clone(&tx)),
            conversation_id: conv_id.to_string(),
        });
    }

    let system_prompt = agent_def["system_prompt"].as_str().unwrap_or("").to_string();
    let mut tool_names: Vec<String> = agent_def["tool_names"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let http_client = reqwest::Client::new();
    let embedding_url = state.daemon.embedding_url.read().await.clone();

    // Parallel pre-pass: skill search + memory prefetch run concurrently.
    let do_skill_pass = !vault_path.is_empty()
        && agent_def["use_skill_pass"].as_bool().unwrap_or(false);
    let do_memory_prefetch = !vault_id.is_empty() && !account_id.is_empty();

    let keywords: Vec<String> = input.split_whitespace()
        .filter(|w| w.chars().count() >= 2)
        .take(5)
        .map(String::from)
        .collect();

    let (skill_result, memory_facts) = tokio::join!(
        async {
            if do_skill_pass {
                Some(super::super::helpers::run_skill_pass(
                    &http_client, &embedding_url, &state.db, &vault_id, &account_id, &input,
                ).await)
            } else {
                None
            }
        },
        async {
            if do_memory_prefetch {
                let facts = super::super::helpers::vault_query_memory_with_limit(
                    &http_client, &embedding_url, &state.db, &vault_id, &account_id, &keywords, 6,
                ).await;
                let fact_ids: Vec<String> = facts.iter()
                    .filter_map(|f| f["fact_id"].as_str().filter(|s| !s.is_empty()))
                    .map(|fid| format!("memory:{}:{}", vault_id, fid))
                    .collect();
                Some((facts, fact_ids))
            } else {
                None
            }
        }
    );

    // Process skill pass result: extract system_injection and meta_functions.
    let (system_injection, meta_functions) = if let Some(skill) = skill_result {
        if skill.skill_titles.is_empty() {
            // No skill matched and use_skill_pass is true → skill discovery mode.
            // Fetch existing skills to give LLM context; enable create_agent_skill.
            let discovery = build_skill_discovery_injection(&state.db, &account_id).await;
            if !tool_names.contains(&"create_agent_skill".to_string()) {
                tool_names.push("create_agent_skill".to_string());
            }
            (discovery, vec![])
        } else {
            if streaming {
                state.daemon.emit("agent:skills_activated", serde_json::json!({
                    "session_id": session_id,
                    "titles": skill.skill_titles,
                    "source": "pre_pass",
                }));
            }
            // trigger_count is bumped only when LLM actually picks the meta_function,
            // not at pre-pass time — so multiple matched skills aren't over-counted.
            (skill.system_injection, skill.meta_functions)
        }
    } else {
        (String::new(), vec![])
    };

    run_interactive_agent(
        state,
        session_id,
        input,
        system_prompt,
        streaming,
        tool_names,
        system_injection,
        activity_context,
        vault_id,
        account_id,
        vault_path,
        conversation_id,
        memory_facts,
        cancel,
        tx,
        meta_functions,
    ).await
}

// ── Pre-planner chain execution ───────────────────────────────────────────────

/// Returns all input params for a tool when it is the first step in a chain.
fn meta_fn_tool_params(tool_name: &str) -> Vec<(&'static str, serde_json::Value)> {
    match tool_name {
        "search_vault" => vec![
            ("query", json!({"type": "string", "description": "搜尋關鍵字"})),
        ],
        "open_note" => vec![
            ("paths", json!({"type": "array", "items": {"type": "string"}, "description": "筆記路徑列表"})),
        ],
        "read_note" => vec![
            ("path", json!({"type": "string", "description": "筆記路徑"})),
        ],
        "update_note" => vec![
            ("path",    json!({"type": "string", "description": "筆記路徑"})),
            ("content", json!({"type": "string", "description": "完整新內容"})),
        ],
        "append_to_note" => vec![
            ("path",    json!({"type": "string", "description": "筆記路徑"})),
            ("content", json!({"type": "string", "description": "要追加的內容"})),
        ],
        "delete_note" => vec![
            ("path", json!({"type": "string", "description": "要刪除的筆記路徑"})),
        ],
        "update_note_frontmatter" => vec![
            ("path",   json!({"type": "string", "description": "筆記路徑"})),
            ("fields", json!({"type": "object", "description": "要更新的 frontmatter 鍵值對，例如 {\"tags\": [\"A\"], \"status\": \"done\"}"})),
        ],
        "list_structure" => vec![
            ("path", json!({"type": "string", "description": "資料夾路徑"})),
        ],
        _ => vec![],
    }
}

/// Returns only the params that cannot be derived from the previous chain step's result.
/// These are the residual user-supplied params for non-first chain tools.
fn meta_fn_residual_params(tool_name: &str) -> Vec<(&'static str, serde_json::Value)> {
    match tool_name {
        // path derived from search result — no residual
        "open_note" | "read_note" | "delete_note" => vec![],
        "append_to_note" => vec![
            ("content", json!({"type": "string", "description": "要追加的內容"})),
        ],
        "update_note" => vec![
            ("content", json!({"type": "string", "description": "完整新內容"})),
        ],
        "update_note_frontmatter" => vec![
            ("fields", json!({"type": "object", "description": "要更新的 frontmatter 鍵值對，例如 {\"tags\": [\"A\"], \"status\": \"done\"}"})),
        ],
        _ => vec![],
    }
}

/// Build the JSON schema for a meta-function.
/// chain[0]'s params are always required; subsequent tools only expose residual params
/// (those that cannot be derived from previous step results).
fn build_meta_fn_schema(spec: &MetaFunctionSpec) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<String> = vec![];

    for (i, tool_name) in spec.chain.iter().enumerate() {
        let params = if i == 0 {
            meta_fn_tool_params(tool_name)
        } else {
            meta_fn_residual_params(tool_name)
        };
        for (param_name, param_schema) in params {
            let key = format!("{}__{}", tool_name, param_name);
            if i == 0 { required.push(key.clone()); }
            properties.insert(key, param_schema);
        }
    }

    json!({
        "type": "function",
        "function": {
            "name": spec.fn_name,
            "description": spec.description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required
            }
        }
    })
}

/// Extract `tool_name__param` → `param` args for a specific tool from LLM-provided args.
pub(crate) fn extract_user_tool_args(tool_name: &str, args: &serde_json::Value) -> serde_json::Value {
    let prefix = format!("{}__", tool_name);
    let mut result = serde_json::Map::new();
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            if let Some(param_name) = k.strip_prefix(&prefix) {
                result.insert(param_name.to_string(), v.clone());
            }
        }
    }
    serde_json::Value::Object(result)
}

/// Derive `to_tool`'s input args from `from_tool`'s result and args.
pub(crate) fn extract_chain_step_args(
    from_tool: &str,
    to_tool: &str,
    from_result: &serde_json::Value,
    from_args: &serde_json::Value,
    user_args: &serde_json::Value,
) -> serde_json::Value {
    match (from_tool, to_tool) {
        ("search_vault", "open_note") | ("list_structure", "open_note") => {
            let paths: Vec<serde_json::Value> = from_result.as_array()
                .map(|a| a.iter()
                    .filter_map(|r| r["path"].as_str().map(|p| json!(p)))
                    .take(1).collect())
                .unwrap_or_default();
            json!({"paths": paths})
        }
        ("search_vault", "read_note") | ("list_structure", "read_note") => {
            let path = from_result.as_array()
                .and_then(|a| a.first())
                .and_then(|r| r["path"].as_str())
                .unwrap_or("");
            json!({"path": path})
        }
        ("read_note", "update_note") => {
            let path = from_args["path"].as_str().unwrap_or("");
            let content = user_args["update_note__content"].as_str().unwrap_or("");
            json!({"path": path, "content": content})
        }
        ("search_vault", "update_note") => {
            let path = from_result.as_array()
                .and_then(|a| a.first())
                .and_then(|r| r["path"].as_str())
                .unwrap_or("");
            let content = user_args["update_note__content"].as_str().unwrap_or("");
            json!({"path": path, "content": content})
        }
        ("search_vault", "append_to_note") | ("list_structure", "append_to_note") => {
            let path = from_result.as_array()
                .and_then(|a| a.first())
                .and_then(|r| r["path"].as_str())
                .unwrap_or("");
            let content = user_args["append_to_note__content"].as_str().unwrap_or("");
            json!({"path": path, "content": content})
        }
        ("search_vault", "delete_note") | ("list_structure", "delete_note") => {
            let path = from_result.as_array()
                .and_then(|a| a.first())
                .and_then(|r| r["path"].as_str())
                .unwrap_or("");
            json!({"path": path})
        }
        ("search_vault", "update_note_frontmatter") | ("list_structure", "update_note_frontmatter") => {
            let path = from_result.as_array()
                .and_then(|a| a.first())
                .and_then(|r| r["path"].as_str())
                .unwrap_or("");
            let fields = user_args["update_note_frontmatter__fields"].clone();
            json!({"path": path, "fields": fields})
        }
        _ => json!({}),
    }
}


/// When use_skill_pass is true but no skill matched the user's input,
/// inject a skill-discovery prompt so LLM can guide the user to select an
/// existing skill or compose a new one with @[tool_name] chain syntax.
async fn build_skill_discovery_injection(db: &crate::db::SurrealDb, account_id: &str) -> String {
    // Fetch existing active skills (title + trigger) for LLM context.
    let existing_skills: Vec<String> = {
        let mut resp = db
            .query("SELECT title, trigger FROM agent_skills WHERE account_id = $aid AND is_active = true ORDER BY trigger_count DESC LIMIT 20")
            .bind(("aid", account_id.to_string()))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<serde_json::Value>>(0).ok())
            .unwrap_or_default();
        resp.iter().map(|s| {
            let title = s["title"].as_str().unwrap_or("").to_string();
            let trigger = s["trigger"].as_str().unwrap_or("").to_string();
            format!("- {title}（觸發詞：{trigger}）")
        }).collect()
    };

    // Known chainable tools users can reference with @[tool_name] syntax.
    let available_tools = [
        "@[search_vault]      — 語意搜尋筆記",
        "@[list_structure]    — 列出資料夾結構",
        "@[read_note]         — 讀取指定筆記",
        "@[open_note]         — 開啟筆記（顯示於 UI）",
        "@[create_note]       — 建立新筆記",
        "@[update_note]       — 更新筆記內容",
        "@[append_to_note]    — 附加內容到筆記",
        "@[delete_note]       — 刪除筆記",
        "@[update_note_frontmatter] — 更新 frontmatter 欄位",
        "@[plan_announce]     — 宣告計畫並等待使用者確認後繼續",
        "@[web_search]        — 網路搜尋",
        "@[think]             — 推理思考（不呼叫外部工具）",
        "@[prefetch_memory]   — 預先載入相關記憶（proactive 模式專用）",
    ];

    let skills_section = if existing_skills.is_empty() {
        "（目前尚無已建立的技能）".to_string()
    } else {
        existing_skills.join("\n")
    };

    format!(
        "## 技能未命中 — 技能探索模式\n\
        使用者的輸入未觸發任何已知技能。請先與使用者確認意圖，\
        引導他選擇現有技能或描述新需求，然後使用 create_agent_skill 工具建立技能。\n\n\
        ### 建立技能的格式規則\n\
        - `behavior` 欄位用自然語言描述行為；如需工具鏈，用 @[tool_name] 標記工具呼叫順序\n\
        - 範例：「先用 @[search_vault] 找到筆記，再用 @[plan_announce] 確認，最後 @[update_note] 更新內容」\n\
        - `injection_mode` 可選 `passive`（依關鍵字觸發）/ `active`（每次都觸發）/ `proactive`（自動背景預載）\n\
        - `trigger` 填寫觸發關鍵詞，多個關鍵詞以逗號分隔\n\n\
        ### 可用工具（@[tool_name] 語法）\n\
        {}\n\n\
        ### 目前已建立的技能\n\
        {}",
        available_tools.join("\n"),
        skills_section,
    )
}

