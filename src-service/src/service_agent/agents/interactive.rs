use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use serde_json::{json, Value};
use crate::api_state::ApiState;
use crate::state::AgentSession;

use super::super::engine::dispatcher::Dispatcher;
use super::super::engine::planner::Planner;
use super::super::engine::tool_registry::ToolRegistry;
use super::super::engine::transaction::{AnswerChannel, Transaction};
use super::super::harness::context_pipeline::{ContextBudget, ContextInput, ContextPipeline};
use super::super::types::{EmitEventFn, NeedConfirmFn, Tool};
use super::super::harness::env::VaultEnv;
use super::super::harness::memory::episodic::EpisodicMemory;
use super::super::harness::memory::working::WorkingMemory;
use super::super::harness::observability::emitter::ObservabilityEmitter;
use super::super::harness::tool_def::{ALL_TOOL_DEFS, build_tools_schema};
use super::super::harness::governance::guard::evaluate_guard;
use super::super::harness::governance::policy::{assert_guard_coverage, build_need_confirm_fn};

/// Run an agent defined by `agent_def` (from DB or inline).
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
    session_id: Option<String>,
) -> String {
    let conv_id    = Arc::new(conversation_id);
    let vault_path = state.resolve_vault_path(&vault_id).await;

    let session_id: Arc<String> = Arc::new(
        session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
    );

    // ── Step 0: Resume / intercept existing session ───────────────────────────
    if try_resume_session(&state, conv_id.as_str(), &input).await {
        return String::new();
    }

    // ── Step 1: Extract agent params ─────────────────────────────────────────
    let enable_think = agent_def["enable_think"].as_bool().unwrap_or(false);
    let max_rounds   = agent_def["max_rounds"].as_u64().unwrap_or(super::super::MAX_ROUNDS as u64) as usize;
    let kind         = agent_def["kind"].as_str().unwrap_or("").to_string();

    // ── Step 2: Resolve tool list ─────────────────────────────────────────────
    let mut tool_names: Vec<String> = agent_def["tool_names"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    // think is never in tool_names; it is injected as a forced Round-0 call via enable_think.
    tool_names.retain(|t| t != "think");
    tool_names = inject_required_tools(tool_names, &kind);

    // ── Step 3: Resolve LLM URL (early exit if not configured) ───────────────
    let llm_url = match state.daemon.llm_url.read().await.clone() {
        Some(u) => u,
        None => {
            state.daemon.emit("llm:done", json!(""));
            return String::new();
        }
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .unwrap_or_default();
    let embedding_url = state.daemon.embedding_url.read().await.clone();

    // ── Step 4: Session registration + AgentEnv assembly ─────────────────────
    // Build AgentEnv first; AgentSession then Arc::clones from env.
    let tx = Arc::new(Transaction::new());
    let env = AgentEnv {
        session_id, conv_id, vault_id, account_id, vault_path,
        cancel: Arc::new(AtomicBool::new(false)),
        answer_channel: Arc::new(AnswerChannel::new()),
        working_memory: WorkingMemory::new(),
        client, llm_url, embedding_url,
        kind, streaming,
        agent_def,
    };
    {
        let mut sessions = state.daemon.agent_sessions.lock().await;
        if let Some(old) = sessions.get(env.conv_id.as_str()) {
            old.cancel.store(true, Ordering::Relaxed);
            if let Some(ref old_tx) = old.transaction {
                let _ = old_tx.cancel().await;
            }
        }
        sessions.insert((*env.conv_id).clone(), AgentSession {
            session_id:     Arc::clone(&env.session_id),
            conv_id:        Arc::clone(&env.conv_id),
            cancel:         Arc::clone(&env.cancel),
            answer_channel: Arc::clone(&env.answer_channel),
            transaction:    Some(Arc::clone(&tx)),
        });
    }

    // ── Step 5+6: Pre-pass (skill search + memory prefetch) + build context ───
    let AgentContextResult {
        messages: mut msgs,
        mem_facts_count,
        activated_skill_titles,
        tool_names,
    } = build_agent_context(&env, &state, &input, activity_context.as_deref(), tool_names).await;

    // ── Step 7: Build emitter + dispatcher ────────────────────────────────────
    let emitter = build_emitter(&env, &state);
    emitter.record_skill_activations(&activated_skill_titles);
    let dispatcher = build_dispatcher(&env, &state, emitter.as_emit_fn());

    // ── Step 8: Tool loop ─────────────────────────────────────────────────────
    let full_response = run_tool_loop(
        &env, &mut msgs, &tool_names,
        enable_think, max_rounds,
        &emitter, &dispatcher, Arc::clone(&tx),
    ).await;

    // ── Step 9: Post-processing ───────────────────────────────────────────────
    post_process(
        &env, &state, &emitter.as_emit_fn(), &emitter,
        &msgs, &full_response, &input, mem_facts_count,
    ).await;

    full_response
}

/// Orchestration-level context for a single agent run.
///
/// Constructed once after Steps 1–4 and passed by reference into every `build_xxx`
/// helper, replacing the ~12 individual variables that were previously threaded through
/// each function signature.
///
/// `tx` (Transaction) is intentionally excluded: it is the confirm/cancel handle
/// passed directly to `dispatcher.run()` and also stored in `AgentSession`.
struct AgentEnv {
    /// Shared with `AgentSession::session_id` via `Arc` — guaranteed identical.
    session_id:    Arc<String>,
    /// Shared with `AgentSession::conv_id` via `Arc` — guaranteed identical.
    conv_id:       Arc<String>,
    vault_id:      String,
    account_id:    String,
    vault_path:    String,
    /// Shared with `AgentSession::cancel` via `Arc`.
    cancel:        Arc<AtomicBool>,
    /// Shared with `AgentSession::answer_channel` via `Arc`.
    answer_channel: Arc<AnswerChannel>,
    working_memory: WorkingMemory,
    client:        reqwest::Client,
    llm_url:       String,
    embedding_url: Option<String>,
    /// Agent kind (e.g. "chat", "background", "sub", "scheduled").
    /// Use `is_background()` / `needs_frontend()` methods rather than comparing kind directly,
    /// so callers remain resilient to new kinds being added.
    kind:          String,
    streaming:     bool,
    /// Raw agent definition from DB — use for lazy field access (use_skill_pass, system_prompt, etc.).
    /// `state` is intentionally excluded: pass `&ApiState` explicitly to each helper that needs it
    /// to avoid coupling this local orchestration struct to the global service state.
    agent_def:     Value,
}

impl AgentEnv {
    /// True for headless agent kinds that have no live frontend listener.
    fn is_background(&self) -> bool {
        matches!(self.kind.as_str(), "background" | "sub" | "scheduled")
    }

    /// True for agent kinds that require a live frontend (ask_user, progress tools).
    fn needs_frontend(&self) -> bool {
        !self.is_background()
    }
}

/// Vault tools whose presence indicates this agent operates on the vault.
/// Used to gate injection of vault-specific utility tools like `get_vault_changes`.
const VAULT_TOOL_MARKERS: &[&str] = &[
    "read_note", "search_vault", "list_structure",
    "create_note", "update_note", "append_to_note", "delete_note",
    "read_then_write", "update_note_frontmatter",
];

/// Auto-inject required tools based on agent kind.
///
/// Tiers:
/// 1. All ReAct agents: introspection + DB-backed utilities (work regardless of kind).
/// 2. Interactive only (non-background kinds): tools that require a live frontend listener.
/// 3. Vault-operating agents: injected when tool list already contains vault tools,
///    regardless of kind.
fn inject_required_tools(mut tool_names: Vec<String>, kind: &str) -> Vec<String> {
    if tool_names.is_empty() { return tool_names; }

    let needs_frontend = !matches!(kind, "background" | "sub" | "scheduled");

    // Tier 1: universal — all ReAct agents.
    for name in [
        "get_session_state",
        "checkpoint", "clear_checkpoint",
        "save_agent_knowledge", "get_agent_knowledge",
        "batch_apply",
    ] {
        if !tool_names.contains(&name.to_string()) {
            tool_names.push(name.to_string());
        }
    }

    // Tier 2: requires frontend listener — interactive agents only.
    if needs_frontend {
        for name in ["ask_user", "progress"] {
            if !tool_names.contains(&name.to_string()) {
                tool_names.push(name.to_string());
            }
        }
    }

    // Tier 3: vault-specific — inject when agent already has vault tools.
    let has_vault_tools = tool_names.iter().any(|t| VAULT_TOOL_MARKERS.contains(&t.as_str()));
    if has_vault_tools && !tool_names.contains(&"get_vault_changes".to_string()) {
        tool_names.push("get_vault_changes".to_string());
    }

    tool_names
}

/// Build an `ObservabilityEmitter` that stamps every SSE payload with `session_id`.
fn build_emitter(env: &AgentEnv, state: &ApiState) -> ObservabilityEmitter {
    let raw_emit: EmitEventFn = {
        let state_c      = state.clone();
        let session_id_c = env.session_id.clone();
        Arc::new(move |event: String, mut payload: Value| {
            if let Value::Object(ref mut m) = payload {
                m.insert("session_id".to_string(), json!(session_id_c));
            }
            state_c.daemon.emit(&event, payload);
        })
    };
    ObservabilityEmitter::new(raw_emit)
}

struct AgentContextResult {
    /// Built LLM message history, ready for the tool loop.
    messages: Vec<Value>,
    /// Number of memory facts injected (for SessionTrace).
    mem_facts_count: usize,
    /// Skill titles activated during pre-pass (for SessionTrace).
    activated_skill_titles: Vec<String>,
    /// Tool list, possibly extended by skill pre-pass.
    tool_names: Vec<String>,
}

/// Run the skill pre-pass + memory prefetch (Step 5) and context pipeline (Step 6) together.
/// `system_prompt` is read from `env.agent_def["system_prompt"]`.
async fn build_agent_context(
    env:              &AgentEnv,
    state:            &ApiState,
    input:            &str,
    activity_context: Option<&str>,
    mut tool_names:   Vec<String>,
) -> AgentContextResult {
    let system_prompt = env.agent_def["system_prompt"].as_str().unwrap_or("").to_string();

    // ── Step 5: Parallel skill pre-pass + memory prefetch ─────────────────────
    // Background agents skip memory prefetch — their input is a fixed admin command,
    // not a user query, so semantic memory lookup produces irrelevant results.
    let do_skill_pass      = !env.vault_path.is_empty() && env.agent_def["use_skill_pass"].as_bool().unwrap_or(false);
    let do_memory_prefetch = env.needs_frontend() && !env.vault_id.is_empty() && !env.account_id.is_empty();
    let keywords: Vec<String> = input.split_whitespace()
        .filter(|w| w.chars().count() >= 2)
        .take(5)
        .map(String::from)
        .collect();

    let (skill_result, prefetched_memory) = tokio::join!(
        async {
            if do_skill_pass {
                Some(super::super::helpers::run_skill_pass(
                    &env.client, &env.embedding_url, &state.db,
                    &env.vault_id, &env.account_id, input,
                ).await)
            } else {
                None
            }
        },
        async {
            if do_memory_prefetch {
                let facts = super::super::helpers::vault_query_memory_with_limit(
                    &env.client, &env.embedding_url, &state.db,
                    &env.vault_id, &env.account_id, &keywords, 6,
                ).await;
                let fact_ids: Vec<String> = facts.iter()
                    .filter_map(|f| f["fact_id"].as_str().filter(|s| !s.is_empty()))
                    .map(|fid| format!("memory:{}:{}", env.vault_id, fid))
                    .collect();
                Some((facts, fact_ids))
            } else {
                None
            }
        }
    );

    // Process skill pass result.
    let mut activated_skill_titles: Vec<String> = Vec::new();
    let system_injection = if let Some(skill) = skill_result {
        if skill.skill_titles.is_empty() {
            let discovery = build_skill_discovery_injection(&state.db, &env.account_id).await;
            if !tool_names.contains(&"create_agent_skill".to_string()) {
                tool_names.push("create_agent_skill".to_string());
            }
            discovery
        } else {
            if env.streaming {
                state.daemon.emit("agent:skills_activated", json!({
                    "session_id": env.session_id,
                    "titles": skill.skill_titles,
                    "source": "pre_pass",
                }));
            }
            activated_skill_titles = skill.skill_titles;
            for t in skill.skill_tool_names {
                if !tool_names.contains(&t) { tool_names.push(t); }
            }
            skill.system_injection
        }
    } else {
        String::new()
    };

    // ── Step 6: Build context ──────────────────────────────────────────────────
    let (mem_facts, mem_fact_ids) = prefetched_memory.unwrap_or_default();
    let pipeline = ContextPipeline::new(ContextBudget::default());
    let built = pipeline.build(
        ContextInput {
            db:               &state.db,
            conv_id:          &env.conv_id,
            vault_id:         &env.vault_id,
            user_input:       input,
            system_prompt:    &system_prompt,
            skill_injection:  &system_injection,
            activity_context,
            memory_facts:     &mem_facts,
            // Citation protocol is only meaningful for streaming chat agents:
            // stream_llm_round parses [cite:...] from SSE output.
            is_chat: env.streaming,
        },
        &env.client,
        &env.llm_url,
    ).await;
    if built.was_trimmed {
        tracing::debug!(
            "[context] history trimmed: before={}chars system={}chars",
            built.history_chars_before_trim,
            built.system_chars_used,
        );
    }
    if env.streaming && !mem_fact_ids.is_empty() {
        state.daemon.emit("memory:prefetched", json!({
            "node_ids": mem_fact_ids,
            "source": "chat",
        }));
    }

    AgentContextResult {
        messages: built.messages,
        mem_facts_count: mem_facts.len(),
        activated_skill_titles,
        tool_names,
    }
}

/// Construct `VaultEnv` from `AgentEnv` + `ApiState`, wire up `need_confirm_fn` + registry,
/// and return a `Dispatcher`.
fn build_dispatcher(env: &AgentEnv, state: &ApiState, emit_fn: EmitEventFn) -> Dispatcher {
    let vault_env = Arc::new(VaultEnv {
        client:        env.client.clone(),
        llm_url:       env.llm_url.clone(),
        db:            state.db.clone(),
        vault_id:      env.vault_id.clone(),
        account_id:    env.account_id.clone(),
        vault_path:    env.vault_path.clone(),
        embedding_url: env.embedding_url.clone(),
        session_id:    (*env.session_id).clone(),
        conv_id:       (*env.conv_id).clone(),
        state:         state.clone(),
        cancel:         Arc::clone(&env.cancel),
        answer_channel: Arc::clone(&env.answer_channel),
        working_memory: env.working_memory.clone(),
        write_snapshots: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        write_mtimes:    Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    });
    let need_confirm_fn: NeedConfirmFn = build_need_confirm_fn(&env.kind);
    let registry = build_dispatcher_registry(Arc::clone(&vault_env));
    Dispatcher::new(Arc::new(registry), emit_fn, need_confirm_fn, env.working_memory.clone())
}

/// Build a ToolRegistry from the static ALL_TOOL_DEFS list.
/// Each closure captures a single `Arc<VaultEnv>` instead of ~10 individual variables.
/// Guard evaluation and post-dispatch SSE are driven by ToolDef metadata.
pub(crate) fn build_dispatcher_registry(env: Arc<VaultEnv>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    // Assert that every destructive write tool has a guard spec or is in the exempt list.
    assert_guard_coverage();

    for def in ALL_TOOL_DEFS {
        let env_exec = Arc::clone(&env);
        let env_rb   = Arc::clone(&env);
        let def      = *def; // ToolDef is Copy

        let execute: super::super::types::ToolFn = Arc::new(move |args: Value| {
            let env = Arc::clone(&env_exec);
            Box::pin(async move {
                // ── Declarative precondition guard ─────────────────────────
                if let Some(ref spec) = def.guard {
                    let hint = env.working_memory.with_records(|store| {
                        evaluate_guard(spec, &args, store)
                    }).await;
                    if let Some(h) = hint {
                        return Ok(json!({
                            "__guard_blocked__": true,
                            "__guard_hint__": h.message,
                            "__guard_required_tool__": h.required_tool,
                            "__guard_required_path__": h.required_path,
                        }));
                    }
                }

                // ── Execute via ToolDef handler ─────────────────────────────
                let result = (def.handler)(Arc::clone(&env), args.clone()).await?;

                // ── Post-dispatch: emit agent:note_refs for read/search tools ─
                let refs = super::super::harness::tools::vault_tools::extract_note_refs(
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

        let rollback: Option<super::super::types::ToolFn> = def.rollback.map(|rb_fn| {
            let env_r = env_rb.clone();
            let boxed: super::super::types::ToolFn = Arc::new(move |args: Value| {
                let env = Arc::clone(&env_r);
                rb_fn(env, args)
            });
            boxed
        });

        registry.register(def.name.to_string(), Tool { execute, rollback });
    }

    registry
}

/// Execute the LLM + tool loop (Path B: ReAct with tools; Path C: pure LLM).
/// Returns the final assistant text response.
async fn run_tool_loop(
    env:          &AgentEnv,
    msgs:         &mut Vec<Value>,
    tool_names:   &[String],
    enable_think: bool,
    max_rounds:   usize,
    emitter:      &ObservabilityEmitter,
    dispatcher:   &Dispatcher,
    tx:           Arc<Transaction>,
) -> String {
    let mut full_response = String::new();

    if !tool_names.is_empty() {
        // ── Path B: ReAct loop ────────────────────────────────────────────────
        let tools_schema = build_tools_schema(tool_names);
        let tools_value  = if tools_schema.is_empty() { None } else { Some(json!(tools_schema)) };
        let think_schema = if enable_think {
            Some(build_tools_schema(&["think".to_string()]))
        } else {
            None
        };

        for round in 0..max_rounds.min(super::super::MAX_ROUNDS) {
            if env.cancel.load(Ordering::Relaxed) { break; }
            emitter.increment_round();

            // Round 0 with enable_think: use think-only schema + forced tool_choice.
            let (round_tools, round_choice) = if round == 0 {
                if let Some(ref ts) = think_schema {
                    (Some(json!(ts)), json!({ "type": "function", "function": { "name": "think" } }))
                } else {
                    (tools_value.clone(), json!("auto"))
                }
            } else {
                (tools_value.clone(), json!("auto"))
            };

            let llm_t0 = std::time::Instant::now();
            let (text, tool_chunks) = if env.streaming {
                let body = match &round_tools {
                    Some(tv) => json!({ "messages": msgs, "tools": tv, "tool_choice": round_choice,
                                       "stream": true, "temperature": 0.7, "max_tokens": 2048 }),
                    None     => json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 2048 }),
                };
                match super::super::harness::tools::vault_tools::stream_llm_round(
                    &env.client, &env.llm_url, body, &emitter.as_emit_fn(),
                    &env.cancel, Some(&env.working_memory),
                ).await {
                    Ok((t, _, chunks)) => (t, chunks),
                    Err(e) => { tracing::warn!("[agent/tools] stream error: {}", e); break; }
                }
            } else {
                match super::super::harness::tools::vault_tools::call_llm_once(
                    &env.client, &env.llm_url, msgs, round_tools, &env.cancel,
                ).await {
                    Ok(r) => r,
                    Err(e) => { tracing::warn!("[agent/tools] llm error: {}", e); break; }
                }
            };
            emitter.record_llm_latency(llm_t0.elapsed().as_millis() as u64);

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
                    Err(e) => { tracing::warn!("[agent/tools] dispatcher error: {}", e); break; }
                };
                msgs.extend(Planner::results_to_messages(&tool_chunks, results));

                // Stall detection: inject warning if any (tool, arg) pair repeated ≥ 2×.
                let repeats = env.working_memory.repeated_calls().await;
                if !repeats.is_empty() {
                    let names: Vec<String> = repeats.iter()
                        .map(|(t, a)| if a.is_empty() { t.clone() } else { format!("{}({})", t, a) })
                        .collect();
                    msgs.push(json!({
                        "role": "system",
                        "content": format!(
                            "[系統提示] 你在本 session 已重複呼叫：{}。請換策略，或呼叫 get_session_state 查看完整執行歷史再決定下一步，或直接根據已取得的資訊回覆使用者。",
                            names.join("、")
                        )
                    }));
                }

                // Context pressure: warn when accumulated msgs exceed char thresholds.
                const CONTEXT_WARN_CHARS: usize     = 28_000; // ~70% of typical 12k-token history budget
                const CONTEXT_CRITICAL_CHARS: usize = 40_000; // ~100% — near limit
                let msgs_chars: usize = msgs.iter()
                    .map(|m| m["content"].as_str().unwrap_or("").len())
                    .sum();
                if msgs_chars >= CONTEXT_CRITICAL_CHARS {
                    msgs.push(json!({
                        "role": "system",
                        "content": "[系統提示] ⚠️ context 已接近上限（約 40,000 字元）。請立即根據已取得的資訊作出最終回覆，不要再呼叫 read_note 或其他大型工具。"
                    }));
                } else if msgs_chars >= CONTEXT_WARN_CHARS {
                    msgs.push(json!({
                        "role": "system",
                        "content": "[系統提示] context 已使用約 70%（28,000+ 字元）。建議改用 search_in_note 取代 read_note 以精準定位段落，避免繼續擴大 context。"
                    }));
                }

                if env.cancel.load(Ordering::Relaxed) { break; }
            } else {
                break;
            }
        }

    } else {
        // ── Path C: pure LLM (no tools) ───────────────────────────────────────
        emitter.increment_round();
        let llm_t0 = std::time::Instant::now();
        if env.streaming {
            match super::super::harness::tools::vault_tools::stream_llm_round(
                &env.client, &env.llm_url,
                json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 2048 }),
                &emitter.as_emit_fn(), &env.cancel, None,
            ).await {
                Ok((text, _, _)) => { full_response = text; }
                Err(e) => { tracing::warn!("[agent/pure] stream error: {}", e); }
            }
        } else {
            match super::super::harness::tools::vault_tools::call_llm_once(
                &env.client, &env.llm_url, msgs, None, &env.cancel,
            ).await {
                Ok((text, _)) => { full_response = text; }
                Err(e) => { tracing::warn!("[agent/pure] llm error: {}", e); }
            }
        }
        emitter.record_llm_latency(llm_t0.elapsed().as_millis() as u64);
    }

    full_response
}

/// Emit final SSE events, persist conversation + trace, and clean up background sessions.
async fn post_process(
    env:             &AgentEnv,
    state:           &ApiState,
    emit_fn:         &EmitEventFn,
    emitter:         &ObservabilityEmitter,
    msgs:            &[Value],
    full_response:   &str,
    input:           &str,
    mem_facts_count: usize,
) {
    if env.streaming {
        (emit_fn)("llm:done".to_string(), json!(full_response));
        if !full_response.is_empty() && super::super::helpers::detect_response_framework(full_response) {
            (emit_fn)("agent:skill_suggestion".to_string(), json!({
                "query": input,
                "response_preview": full_response.chars().take(200).collect::<String>(),
            }));
        }
    }

    // Persist conversation to DB via EpisodicMemory.
    // Only user + assistant text messages; tool_call/tool_result rows are ephemeral.
    {
        let mut to_save: Vec<Value> = msgs.iter()
            .filter(|m| matches!(m["role"].as_str(), Some("user") | Some("assistant")))
            .filter(|m| m["tool_calls"].is_null())
            .filter(|m| !m["content"].as_str().unwrap_or("").is_empty())
            .cloned()
            .collect();
        if !full_response.is_empty() {
            to_save.push(json!({ "role": "assistant", "content": full_response }));
        }
        let episodic = EpisodicMemory::new(state.db.clone());
        episodic.append(&env.conv_id, &to_save).await;
        episodic.set_title_if_blank(&env.conv_id, &to_save).await;
    }

    // Assemble + emit session trace, then persist to DB.
    let trace = emitter.finish(&env.working_memory, &env.conv_id, mem_facts_count).await;
    tracing::debug!(
        "[session:trace] session={} rounds={} tools={} blocked={} tool_ms={}ms",
        env.session_id, trace.round_count, trace.total_calls(), trace.blocked_calls(), trace.total_tool_ms(),
    );
    super::super::harness::observability::trace_store::save_session_trace(
        &state.db, &env.vault_id, &env.account_id, &trace,
    ).await;

    // Background agents use a one-shot UUID conversation_id; clean up immediately.
    if env.is_background() {
        cleanup_session(state, &env.conv_id).await;
    }
}

/// Returns `true` if `input` was consumed by a suspended session (caller should return early).
///
/// Two resume paths, checked in order:
/// 1. `ask_user` waiting — forward text via `AnswerChannel`.
/// 2. Write confirmation pending — classify intent; commit/cancel `Transaction` and return true
///    only for clear Confirm/Cancel/Interrupt (ambiguous input falls through to a new run).
async fn try_resume_session(state: &ApiState, conv_id: &str, input: &str) -> bool {
    let (pending_tx, answer_channel) = {
        let sessions = state.daemon.agent_sessions.lock().await;
        let s = sessions.get(conv_id);
        (
            s.and_then(|s| s.transaction.clone()),
            s.map(|s| Arc::clone(&s.answer_channel)),
        )
    };

    if let Some(ch) = answer_channel {
        if ch.try_resume(input.to_string()).await {
            return true;
        }
    }

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
                Some((cc, ccl, ci)) => classifier.classify_with_centroids(input, &embed_fn, &cc, &ccl, &ci).await,
                None => classifier.classify(input).await,
            };
            use super::super::engine::intent_classifier::Intent;
            match intent {
                Intent::Confirm => { let _ = pending.commit().await; }
                Intent::Cancel | Intent::Interrupt => { let _ = pending.cancel().await; }
                _ => {}
            }
            if matches!(intent, Intent::Confirm | Intent::Cancel | Intent::Interrupt) {
                return true;
            }
        }
    }

    false
}

pub async fn cleanup_session(state: &ApiState, conv_id: &str) {
    let mut sessions = state.daemon.agent_sessions.lock().await;
    sessions.remove(conv_id);
}

/// When use_skill_pass is true but no skill matched the user's input,
/// inject a skill-discovery prompt so LLM can guide the user to select an
/// existing skill or compose a new one with @[tool_name] chain syntax.
async fn build_skill_discovery_injection(db: &crate::db::SurrealDb, account_id: &str) -> String {
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
