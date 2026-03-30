use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use serde_json::{json, Value};
use crate::api_state::ApiState;
use crate::db::SurrealDb;
use crate::state::AgentSession;

use super::dispatcher::Dispatcher;
use super::planner::Planner;
use super::tool_registry::ToolRegistry;
use super::transaction::Transaction;
use super::types::{EmitEventFn, IsWriteFn, Tool};

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
pub async fn run_interactive_agent(
    state: ApiState,
    session_id: String,
    input: String,
    messages: Vec<Value>,          // fallback when no conversation_id
    system: String,
    use_tools: bool,
    activity_context: Option<String>,
    vault_id: String,
    account_id: String,
    vault_path: String,
    conversation_id: String,
) {
    let conv_id = conversation_id;

    // 0. Write-confirmation intercept:
    //    If the same conversation already has a session with a Preparing transaction,
    //    classify input intent and commit/cancel that transaction instead of
    //    starting a new LLM round.
    {
        let pending_tx: Option<Arc<Transaction>> = {
            let sessions = state.daemon.agent_sessions.lock().await;
            sessions.get(&conv_id).and_then(|s| s.transaction.clone())
        };
        if let Some(pending) = pending_tx {
            use super::transaction::TransactionState;
            if pending.state().await == TransactionState::Preparing {
                let embed_fn: super::types::EmbedFn = {
                    let client = reqwest::Client::new();
                    let llm_url = state.daemon.llm_url.read().await.clone().unwrap_or_default();
                    Arc::new(move |text: String| {
                        let client = client.clone();
                        let llm_url = llm_url.clone();
                        Box::pin(async move {
                            super::helpers::embed_text_llm(&client, &llm_url, &text).await
                        })
                    })
                };
                let classifier = super::intent_classifier::IntentClassifier::new();
                let intent = match super::intent_classifier::IntentClassifier::compute_centroids_cached(
                    &state.daemon.intent_centroids,
                    &embed_fn,
                ).await {
                    Some((cc, ccl, ci)) => classifier.classify_with_centroids(&input, &embed_fn, &cc, &ccl, &ci).await,
                    None => classifier.classify(&input).await,
                };
                use super::intent_classifier::Intent;
                match intent {
                    Intent::Confirm => { let _ = pending.commit().await; }
                    Intent::Cancel | Intent::Interrupt => { let _ = pending.cancel().await; }
                    _ => {} // Unrelated new question — fall through to start a fresh LLM round
                }
                if matches!(intent, Intent::Confirm | Intent::Cancel | Intent::Interrupt) {
                    return;
                }
            }
        }
    }

    // 1. Register session in map (keyed by conversation_id).
    //    If a previous session for this conversation exists, cancel it first.
    let cancel = Arc::new(AtomicBool::new(false));
    let tx = Arc::new(Transaction::new());
    {
        let mut sessions = state.daemon.agent_sessions.lock().await;
        if let Some(old) = sessions.get(&conv_id) {
            old.cancel.store(true, Ordering::Relaxed);
            if let Some(ref old_tx) = old.transaction {
                let _ = old_tx.cancel().await;
            }
        }
        sessions.insert(conv_id.clone(), AgentSession {
            session_id: session_id.clone(),
            cancel: Arc::clone(&cancel),
            transaction: Some(Arc::clone(&tx)),
            conversation_id: conv_id.clone(),
        });
    }

    // 2. Resolve llm_url
    let llm_url = match state.daemon.llm_url.read().await.clone() {
        Some(u) => u,
        None => {
            state.daemon.emit("llm:done", json!(""));
            return;
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .unwrap_or_default();
    let embedding_url = state.daemon.embedding_url.read().await.clone();

    // 3. Load messages from DB; fall back to frontend-passed messages if DB returns empty.
    let mut messages_json: Vec<Value> = {
        let db_msgs = super::helpers::load_messages_db(&state.db, &conv_id).await;
        let mut v = if db_msgs.is_empty() { messages.clone() } else { db_msgs };
        if v.last().and_then(|m| m["role"].as_str()) != Some("user") {
            v.push(json!({"role": "user", "content": input}));
        }
        v
    };

    // 4. Assemble system prompt
    let anti_hallucination = "\n\n必須實際呼叫工具完成任務；禁止假裝或虛構結果。\
                               若搜尋無結果，直接說明找不到。\
                               回覆中引用筆記時，請包含完整的 vault 相對路徑。";
    let sys_with_activity = if let Some(ref ac) = activity_context {
        format!("{}\n\n[使用者活動紀錄]\n{}", system, ac)
    } else {
        system.clone()
    };
    let sys_content = format!("{}{}", sys_with_activity, anti_hallucination);

    if messages_json.first().and_then(|m| m["role"].as_str()) == Some("system") {
        messages_json[0] = json!({"role": "system", "content": sys_content});
    } else {
        messages_json.insert(0, json!({"role": "system", "content": sys_content}));
    }

    // 5. Skill pre-pass + tool selection
    let mut tool_names: Vec<String> = vec!["search_skills".to_string(), "plan_announce".to_string()];

    if use_tools && !vault_path.is_empty() {
        let matched = super::helpers::search_skills_db(&state.db, &account_id, &input).await;

        if !matched.is_empty() {
            let mut proactive_parts: Vec<String> = vec![];
            for (_, _, _, _, need_chain, chain_order, mode) in &matched {
                if mode != "proactive" || !*need_chain { continue; }
                for tool_name in chain_order {
                    if tool_name == "prefetch_memory" {
                        let kw_input: String = input.chars().take(120).collect();
                        let keywords = if kw_input.is_empty() { vec![] } else { vec![kw_input.clone()] };
                        let facts = super::helpers::vault_query_memory_with_limit(&state.db, &vault_id, &account_id, &keywords, 8).await;
                        if !facts.is_empty() {
                            let lines: Vec<String> = facts.iter().map(|r| {
                                let cat = r["category"].as_str().unwrap_or("general");
                                let content = r["content"].as_str().unwrap_or("");
                                format!("[{}] {}", cat, content)
                            }).collect();
                            proactive_parts.push(format!("## 相關記憶\n{}", lines.join("\n")));
                        }
                    }
                }
            }
            let proactive_context = proactive_parts.join("\n\n");

            let skill_text: String = matched.iter()
                .filter(|(_, _, beh, _, _, _, mode)| !beh.is_empty() && mode != "proactive")
                .map(|(_, title, beh, _, need_chain, chain_order, _)| {
                    if *need_chain && !chain_order.is_empty() {
                        format!("[技能：{}]\n{}\n工具執行順序：{}", title, beh, chain_order.join(" → "))
                    } else {
                        format!("[技能：{}]\n{}", title, beh)
                    }
                })
                .collect::<Vec<_>>().join("\n\n");

            let skill_titles: Vec<String> = matched.iter().map(|(_, t, _, _, _, _, _)| t.clone()).collect();

            let mut required_tools: Vec<String> = matched.iter()
                .flat_map(|(_, _, _, tc, _, _, _)| tc.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            required_tools.sort();

            for (skill_id, _, _, _, _, _, _) in &matched {
                let sid = skill_id.clone();
                let db = state.db.clone();
                let now = chrono::Utc::now().timestamp();
                tokio::spawn(async move {
                    let _ = db
                        .query("UPDATE agent_skills SET trigger_count = (trigger_count OR 0) + 1, last_triggered_at = $now WHERE skill_id = $sid")
                        .bind(("now", now))
                        .bind(("sid", sid))
                        .await;
                });
            }

            if let Some(sys) = messages_json.first_mut() {
                if sys["role"].as_str() == Some("system") {
                    let existing = sys["content"].as_str().unwrap_or("").to_string();
                    let mut injections = vec![existing];
                    if !proactive_context.is_empty() {
                        injections.push(proactive_context.chars().take(1000).collect());
                    }
                    if !skill_text.is_empty() {
                        injections.push(skill_text.chars().take(1500).collect());
                    }
                    *sys = json!({"role": "system", "content": injections.join("\n\n")});
                }
            }

            if !skill_titles.is_empty() {
                state.daemon.emit("agent:skills_activated", json!({
                    "session_id": session_id,
                    "titles": skill_titles,
                }));
            }

            if !required_tools.is_empty() {
                tool_names = required_tools;
            }
        }

        // 6. Context sliding window (12000 chars)
        {
            const MAX_HISTORY_CHARS: usize = 12000;
            let system_part: Vec<Value> = messages_json.iter()
                .filter(|m| m["role"].as_str() == Some("system"))
                .cloned().collect();
            let hist: Vec<Value> = messages_json.into_iter()
                .filter(|m| m["role"].as_str() != Some("system"))
                .collect();
            let total: usize = hist.iter()
                .map(|m| m["content"].as_str().unwrap_or("").len())
                .sum();
            let trimmed = if total > MAX_HISTORY_CHARS {
                let mut chars = total;
                let mut drop_n = 0usize;
                while chars > MAX_HISTORY_CHARS && drop_n + 4 < hist.len() {
                    chars = chars.saturating_sub(hist[drop_n]["content"].as_str().unwrap_or("").len());
                    drop_n += 1;
                }
                hist[drop_n..].to_vec()
            } else {
                hist
            };
            messages_json = system_part.into_iter().chain(trimmed.into_iter()).collect();
        }
    }

    // 7. Build ToolRegistry + Dispatcher
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
        super::vault_tools::is_interactive_write_tool(name)
    });

    let registry = build_interactive_registry(
        &client, &llm_url, &state.db,
        &vault_id, &account_id, &vault_path, &embedding_url,
        &session_id, &state,
    );

    let dispatcher = Dispatcher::new(
        Arc::new(registry),
        Arc::clone(&emit_fn_closure),
        Arc::clone(&is_write_fn),
    );

    // 8. Tool loop or no-tool LLM
    let tools_schema = super::vault_tools::build_tools_schema_interactive(&tool_names);
    let mut msgs = messages_json.clone();
    let mut full_response = String::new();

    if use_tools && !vault_path.is_empty() {
        for _round in 0..super::MAX_ROUNDS {
            if cancel.load(Ordering::Relaxed) { break; }

            let body = if tools_schema.is_empty() {
                json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 2048 })
            } else {
                json!({
                    "messages": msgs,
                    "tools": tools_schema,
                    "tool_choice": "auto",
                    "stream": true,
                    "temperature": 0.7,
                    "max_tokens": 2048,
                })
            };

            let (text, finish_reason, tool_chunks) = match super::vault_tools::stream_llm_round(
                &client, &llm_url, body, &state, &session_id, &cancel,
            ).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("[interactive] stream error: {}", e);
                    break;
                }
            };

            if !text.is_empty() {
                full_response = text.clone();
            }

            if finish_reason == "tool_calls" && !tool_chunks.is_empty() {
                // Build assistant message
                let tc_json: Vec<Value> = tool_chunks.iter().map(|tc| json!({
                    "id": tc.0, "type": "function",
                    "function": { "name": tc.1, "arguments": tc.2 },
                })).collect();
                msgs.push(json!({ "role": "assistant", "content": null, "tool_calls": tc_json }));

                let graph = Planner::plan_from_chunks(&tool_chunks);
                let results = match dispatcher.run(Arc::clone(&tx), graph).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("[interactive] dispatcher error: {}", e);
                        break;
                    }
                };

                msgs.extend(Planner::results_to_messages(&tool_chunks, results));

                // Check if cancelled after this round
                if cancel.load(Ordering::Relaxed) { break; }
            } else {
                break;
            }
        }
    } else {
        // No vault / use_tools=false → pure LLM streaming, no tools
        let body = json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 2048 });
        match super::vault_tools::stream_llm_round(&client, &llm_url, body, &state, &session_id, &cancel).await {
            Ok((text, _, _)) => { full_response = text; }
            Err(e) => { tracing::warn!("[interactive] no-vault stream error: {}", e); }
        }
    }

    state.daemon.emit("llm:done", json!(full_response));

    // 9. Skill suggestion detection
    if !full_response.is_empty() && super::helpers::detect_response_framework(&full_response) {
        state.daemon.emit("agent:skill_suggestion", json!({
            "query": input,
            "response_preview": full_response.chars().take(200).collect::<String>(),
        }));
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

}

/// Build a ToolRegistry where every closure calls `dispatch_interactive_tool`.
/// Special tools (plan_announce, open_note) emit their own SSE from within the closure.
fn build_interactive_registry(
    client: &reqwest::Client,
    llm_url: &str,
    db: &crate::db::SurrealDb,
    vault_id: &str,
    account_id: &str,
    vault_path: &str,
    embedding_url: &Option<String>,
    session_id: &str,
    state: &ApiState,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    let all_tool_names = [
        "list_structure", "read_note", "create_note", "update_note", "create_folder",
        "search_vault", "query_memory", "delete_note", "delete_folder", "move_note",
        "append_to_note", "search_skills", "create_agent_skill", "plan_announce",
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

        let execute: super::types::ToolFn = Arc::new(move |args: Value| {
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

                let result = super::vault_tools::dispatch_interactive_tool(
                    &client, &llm_url, &db,
                    &vault_id, &account_id, &vault_path, &embedding_url,
                    &name, &args,
                ).await;

                match result {
                    Ok((v, _rollback)) => {
                        // Post-dispatch: emit note_refs
                        let refs = super::vault_tools::extract_note_refs(&name, &args, &v, &vault_path);
                        if !refs.is_empty() {
                            state_c.daemon.emit("agent:note_refs", json!({
                                "session_id": session_id,
                                "paths": refs,
                            }));
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
