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
use super::super::harness::env::VaultEnv;
use super::super::harness::tool_def::{ALL_TOOL_DEFS, GuardLevel, build_tools_schema};

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
    tool_names: Vec<String>,       // pre-resolved by caller; must NOT contain "think"
    enable_think: bool,            // if true, force Round 0 to call think via tool_choice
    system_injection: String,      // extra text appended to system prompt (from skill pass)
    activity_context: Option<String>,
    vault_id: String,
    account_id: String,
    conversation_id: String,
    // Memory facts pre-fetched by run_agent in parallel with skill_pass.
    // When Some, skip the in-body fetch (step 4b). When None, fetch here (legacy / direct callers).
    prefetched_memory: Option<(Vec<serde_json::Value>, Vec<String>)>,
    // Session cancel flag and transaction — created in run_agent and passed in.
    cancel: Arc<AtomicBool>,
    tx: Arc<Transaction>,
    // Per-session tool execution evidence store (shared with AgentSession).
    tool_calls_store: Arc<tokio::sync::Mutex<std::collections::HashMap<String, crate::state::ToolCallRecord>>>,
) -> String {
    let conv_id = conversation_id;
    let vault_path = state.resolve_vault_path(&vault_id).await;

    // 2. Resolve llm_url
    let llm_url = match state.daemon.llm_url.read().await.clone() {
        Some(u) => u,
        None => {
            state.daemon.emit("llm:done", json!(""));
            return String::new();
        }
    };

    let tool_names = tool_names;

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

    // 4. Inject memory facts (always from pre-fetched parallel pass)
    let (mem_facts, mem_fact_ids) = prefetched_memory.unwrap_or_default();
    inject_memory(&mut messages_json, &mem_facts);
    if streaming && !mem_fact_ids.is_empty() {
        state.daemon.emit("memory:prefetched", json!({
            "node_ids": mem_fact_ids,
            "source": "chat",
        }));
    }

    // 5. Trim context window (summarize oldest messages when over limit)
    trim_context(&mut messages_json, &client, &llm_url).await;

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

    // Build VaultEnv once; all tool handlers share it via Arc.
    let env = Arc::new(VaultEnv {
        client:           client.clone(),
        llm_url:          llm_url.clone(),
        db:               state.db.clone(),
        vault_id:         vault_id.clone(),
        account_id:       account_id.clone(),
        vault_path:       vault_path.clone(),
        embedding_url:    embedding_url.clone(),
        session_id:       session_id.clone(),
        state:            state.clone(),
        cancel:           Arc::clone(&cancel),
        tool_calls_store: Arc::clone(&tool_calls_store),
    });

    let is_write_fn: IsWriteFn = Arc::new(|name: &str| {
        // Derive is_write from the ToolDef registry — single source of truth.
        super::super::harness::tool_def::find_tool_def(name)
            .map(|d| d.is_write)
            .unwrap_or(false)
    });

    let registry = build_interactive_registry(Arc::clone(&env));

    let dispatcher = Dispatcher::new(
        Arc::new(registry),
        Arc::clone(&emit_fn_closure),
        Arc::clone(&is_write_fn),
        Arc::clone(&tool_calls_store),
    );

    // 7. Tool loop:
    //    B) tool_names non-empty: ReAct path (plan_from_chunks, verified_paths enforced in tools)
    //    C) no tools: pure LLM
    let mut msgs = messages_json.clone();
    let mut full_response = String::new();

    // ── Path B: direct tools (ReAct / plan_from_chunks) ──────────────────────
    if !tool_names.is_empty() {
        let tools_schema = build_tools_schema(&tool_names);
        let tools_value = if tools_schema.is_empty() { None } else { Some(json!(tools_schema)) };

        // When enable_think=true, force Round 0 to call think via tool_choice.
        // think is NOT in tool_names so subsequent rounds cannot call it.
        let think_schema = if enable_think {
            Some(build_tools_schema(&["think".to_string()]))
        } else {
            None
        };

        for round in 0..super::super::MAX_ROUNDS {
            if cancel.load(Ordering::Relaxed) { break; }

            // Round 0 with enable_think: use think-only schema + forced tool_choice.
            // Subsequent rounds use the normal tools schema with tool_choice: auto.
            let (round_tools, round_choice) = if round == 0 {
                if let Some(ref ts) = think_schema {
                    (Some(json!(ts)), json!({ "type": "function", "function": { "name": "think" } }))
                } else {
                    (tools_value.clone(), json!("auto"))
                }
            } else {
                (tools_value.clone(), json!("auto"))
            };

            let (text, tool_chunks) = if streaming {
                let body = match &round_tools {
                    Some(tv) => json!({ "messages": msgs, "tools": tv, "tool_choice": round_choice,
                                       "stream": true, "temperature": 0.7, "max_tokens": 2048 }),
                    None     => json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 2048 }),
                };
                match super::super::tools::vault_tools::stream_llm_round(
                    &client, &llm_url, body, &state, &session_id, &cancel, Some(&tool_calls_store),
                ).await {
                    Ok((t, _, chunks)) => (t, chunks),
                    Err(e) => { tracing::warn!("[interactive/tools] stream error: {}", e); break; }
                }
            } else {
                match super::super::tools::vault_tools::call_llm_once(
                    &client, &llm_url, &msgs, round_tools, &cancel,
                ).await {
                    Ok(r) => r,
                    Err(e) => { tracing::warn!("[interactive/tools] llm error: {}", e); break; }
                }
            };

            if !text.is_empty() { full_response = text; }

            if !tool_chunks.is_empty() {
                let tc_json: Vec<Value> = tool_chunks.iter().map(|tc| json!({
                    "id": tc.0, "type": "function",
                    "function": { "name": tc.1, "arguments": tc.2 },
                })).collect();
                msgs.push(json!({ "role": "assistant", "content": null, "tool_calls": tc_json }));
                let graph = Planner::plan_from_chunks(&tool_chunks);
                let results = match dispatcher.run(Arc::clone(&tx), graph).await {
                    Ok(r) => r,
                    Err(e) => { tracing::warn!("[interactive/tools] dispatcher error: {}", e); break; }
                };
                msgs.extend(Planner::results_to_messages(&tool_chunks, results));
                if cancel.load(Ordering::Relaxed) { break; }
            } else {
                break;
            }
        }

    // ── Path C: pure LLM (no tools / no vault) ────────────────────────────────
    } else {
        if streaming {
            match super::super::tools::vault_tools::stream_llm_round(
                &client, &llm_url,
                json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 2048 }),
                &state, &session_id, &cancel, None, // Path C: pure chat, no citation validation
            ).await {
                Ok((text, _, _)) => { full_response = text; }
                Err(e) => { tracing::warn!("[interactive/pure] stream error: {}", e); }
            }
        } else {
            match super::super::tools::vault_tools::call_llm_once(&client, &llm_url, &msgs, None, &cancel).await {
                Ok((text, _)) => { full_response = text; }
                Err(e) => { tracing::warn!("[interactive/pure] llm error: {}", e); }
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
        // Only persist user + assistant text messages; tool_call/tool_result rows are
        // ephemeral within a single turn and can be large (full note content).
        // The assistant's final text already captures what matters from tool results.
        let mut to_save: Vec<Value> = msgs.iter()
            .filter(|m| matches!(m["role"].as_str(), Some("user") | Some("assistant")))
            .filter(|m| m["tool_calls"].is_null())
            .filter(|m| !m["content"].as_str().unwrap_or("").is_empty())
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

/// Build a ToolRegistry from the static ALL_TOOL_DEFS list.
/// Each closure captures a single `Arc<VaultEnv>` instead of ~10 individual variables.
/// Guard evaluation and post-dispatch SSE are driven by ToolDef metadata.
pub(crate) fn build_interactive_registry(env: Arc<VaultEnv>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    for def in ALL_TOOL_DEFS {
        let env_c = Arc::clone(&env);
        let def   = *def; // ToolDef is Copy

        let execute: super::super::types::ToolFn = Arc::new(move |args: Value| {
            let env = Arc::clone(&env_c);
            Box::pin(async move {
                // ── Declarative precondition guard ─────────────────────────
                if let Some(spec) = def.guard {
                    let raw_path = (spec.path_extractor)(&args);
                    if raw_path.is_empty() {
                        return Ok(json!("路徑參數不能為空，請提供有效的路徑後再試。"));
                    }
                    let target = if spec.is_folder {
                        raw_path.to_lowercase()
                    } else {
                        norm_path(&raw_path)
                    };
                    let store = env.tool_calls_store.lock().await;
                    let path_ok      = check_path_seen(&store, &target, spec.is_folder);
                    let content_ok   = !matches!(spec.require, GuardLevel::ContentRead)
                        || check_content_read(&store, &target);
                    let was_searched = has_search_result(&store, spec.is_folder);
                    drop(store);

                    if !path_ok {
                        let hint = if spec.is_folder {
                            if was_searched {
                                format!("list_structure 結果中找不到資料夾 '{}'，請確認名稱是否正確。", raw_path)
                            } else {
                                format!("資料夾 '{}' 尚未驗證存在，請先呼叫 list_structure 確認。", raw_path)
                            }
                        } else if was_searched {
                            format!("搜尋結果中找不到 '{}'，請確認筆記名稱或換個關鍵字再搜尋。", raw_path)
                        } else {
                            format!("路徑 '{}' 尚未驗證存在，請先使用 search_vault 或 list_structure 確認。", raw_path)
                        };
                        return Ok(json!(hint));
                    }
                    if !content_ok {
                        return Ok(json!(format!(
                            "尚未成功讀取 '{}' 的內容（讀取失敗或未呼叫 read_note）。請先呼叫 read_note 確認內容後再修改。",
                            raw_path
                        )));
                    }
                }

                // ── Execute via ToolDef handler ─────────────────────────────
                let result = (def.handler)(Arc::clone(&env), args.clone()).await?;

                // ── Post-dispatch: emit agent:note_refs for read/search tools ─
                let refs = super::super::tools::vault_tools::extract_note_refs(
                    def.name, &args, &result, &env.vault_path,
                );
                if !refs.is_empty() {
                    env.state.daemon.emit("agent:note_refs", json!({
                        "session_id": env.session_id,
                        "paths": refs,
                    }));
                }

                Ok(result)
            })
        });

        registry.register(def.name.to_string(), Tool { execute, rollback: None });
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
    conversation_id: String,
    streaming: bool,
    activity_context: Option<String>,
) -> String {
    let vault_path = state.resolve_vault_path(&vault_id).await;
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

    // 1. Register session (cancel flag + transaction + tool_calls evidence store).
    //    Cancel any existing session for this conversation first.
    let cancel = Arc::new(AtomicBool::new(false));
    let tx = Arc::new(Transaction::new());
    let tool_calls_store: Arc<tokio::sync::Mutex<std::collections::HashMap<String, crate::state::ToolCallRecord>>> =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
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
            tool_calls: Arc::clone(&tool_calls_store),
        });
    }

    let system_prompt = agent_def["system_prompt"].as_str().unwrap_or("").to_string();
    let enable_think = agent_def["enable_think"].as_bool().unwrap_or(false);
    let mut tool_names: Vec<String> = agent_def["tool_names"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    // think is never in tool_names; it is injected as a forced Round-0 call via enable_think.
    tool_names.retain(|t| t != "think");

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

    // Process skill pass result: extract system_injection and add skill's chain tools.
    let system_injection = if let Some(skill) = skill_result {
        if skill.skill_titles.is_empty() {
            // No skill matched and use_skill_pass is true → skill discovery mode.
            // Fetch existing skills to give LLM context; enable create_agent_skill.
            let discovery = build_skill_discovery_injection(&state.db, &account_id).await;
            if !tool_names.contains(&"create_agent_skill".to_string()) {
                tool_names.push("create_agent_skill".to_string());
            }
            discovery
        } else {
            if streaming {
                state.daemon.emit("agent:skills_activated", serde_json::json!({
                    "session_id": session_id,
                    "titles": skill.skill_titles,
                    "source": "pre_pass",
                }));
            }
            // Add tools required by skill chains to the agent's direct tool access.
            for t in skill.skill_tool_names {
                if !tool_names.contains(&t) {
                    tool_names.push(t);
                }
            }
            skill.system_injection
        }
    } else {
        String::new()
    };

    run_interactive_agent(
        state,
        session_id,
        input,
        system_prompt,
        streaming,
        tool_names,
        enable_think,
        system_injection,
        activity_context,
        vault_id,
        account_id,
        conversation_id,
        memory_facts,
        cancel,
        tx,
        tool_calls_store,
    ).await
}

/// Returns true if a read_note result value indicates a failure (file not found, empty vault, etc.).
/// vault_tools::vault_read_note always returns Ok(String), so errors are encoded as specific prefixes.
fn is_read_note_error(result: &serde_json::Value) -> bool {
    match result.as_str() {
        Some(s) => s.starts_with("讀取失敗：") || s == "Vault 未設定" || s == "路徑為空" || s.is_empty(),
        None => true, // unexpected type → treat as error
    }
}

// ── Declarative tool guard ──────────────────────────────────────────────────────

/// Normalize a vault path: lowercase first, then ensure .md suffix.
/// Lowercasing before the ends_with check avoids "FOO.MD" → "foo.md.md" double-extension.
fn norm_path(p: &str) -> String {
    let lower = p.to_lowercase();
    if lower.ends_with(".md") { lower } else { format!("{}.md", lower) }
}

type StoreMap = std::collections::HashMap<String, crate::state::ToolCallRecord>;

/// Check whether `target` (already normalized) appears in any prior tool's evidence.
/// `is_folder`: skip note-only sources and use text substring for list_structure.
fn check_path_seen(store: &StoreMap, target: &str, is_folder: bool) -> bool {
    store.values().any(|rec| match rec.name.as_str() {
        "search_vault" => {
            // Returns JSON array [{path, title}]; only relevant for notes.
            !is_folder && rec.result.as_array().map(|a| a.iter().any(|r|
                r["path"].as_str().map(|p| norm_path(p) == target).unwrap_or(false)
            )).unwrap_or(false)
        }
        "list_structure" => {
            // Returns plain text (indented tree).
            // Notes: check exact path substring ("notes/foo.md").
            // Folders: check "foldername/" to avoid prefix false-positives
            //   e.g. "note" must NOT match "notes/foo.md", only "note/" would match "note/foo.md".
            rec.result.as_str().map(|text| {
                let text_lower = text.to_lowercase();
                if is_folder {
                    text_lower.contains(&format!("{}/", target))
                } else {
                    text_lower.contains(target)
                }
            }).unwrap_or(false)
        }
        "read_note" => {
            !is_folder
            && rec.args["path"].as_str().map(|p| norm_path(p) == target).unwrap_or(false)
            && !is_read_note_error(&rec.result)
        }
        "open_note" => {
            !is_folder && (
                rec.args["path"].as_str().map(|p| norm_path(p) == target).unwrap_or(false)
                || rec.args["paths"].as_array().map(|a| a.iter().any(|p|
                    p.as_str().map(|p| norm_path(p) == target).unwrap_or(false)
                )).unwrap_or(false)
            )
        }
        _ => false,
    })
}

/// Check whether read_note succeeded for `target` (non-error content exists in store).
fn check_content_read(store: &StoreMap, target: &str) -> bool {
    store.values().any(|rec|
        rec.name == "read_note"
        && rec.args["path"].as_str().map(|p| norm_path(p) == target).unwrap_or(false)
        && !is_read_note_error(&rec.result)
    )
}

/// Check whether a relevant discovery tool was called (to give better error hints).
/// For folder guards only `list_structure` counts; for note guards either counts.
fn has_search_result(store: &StoreMap, is_folder: bool) -> bool {
    store.values().any(|rec| {
        if is_folder {
            rec.name == "list_structure"
        } else {
            matches!(rec.name.as_str(), "search_vault" | "list_structure")
        }
    })
}

/// When use_skill_pass is true but no skill matched the user's input,
/// inject a skill-discovery prompt so LLM can guide the user to select an
/// existing skill or compose a new one with @[tool_name] chain syntax.
async fn build_skill_discovery_injection(db: &crate::db::SurrealDb, account_id: &str) -> String {
    // Fetch existing active skills (title + trigger) for LLM context.
    let existing_skills: Vec<String> = {
        let resp = db
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

