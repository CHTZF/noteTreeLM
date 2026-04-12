use std::sync::Arc;
use std::sync::atomic::Ordering;
use serde_json::{json, Value};
use super::super::harness::runtime::{HarnessRequestRuntime, AgentContextResult};
use super::super::types::AgentSession;

use super::super::harness::engine::planner::Planner;
use super::super::harness::engine::transaction::Transaction;
use super::super::harness::tool_def::build_tools_schema;
use super::super::harness::governance::guard::GuardOutcome;

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
    mut runtime: HarnessRequestRuntime,
    input: String,
    activity_context: Option<String>,
) -> String {
    // ── Step 0: Resume / intercept existing session ───────────────────────────
    if runtime.try_resume(&input).await {
        return String::new();
    }
    let session_t0 = std::time::Instant::now();
    let pre_llm_t0 = std::time::Instant::now();

    // ── Step 1: Extract agent params ─────────────────────────────────────────
    // native_think models (e.g. Qwen3.5) produce <think> blocks natively —
    // the think tool is redundant and wastes a full round when the LLM ignores it.
    let enable_think = runtime.agent_def["enable_think"].as_bool().unwrap_or(false)
        && !runtime.native_think;
    let max_rounds   = runtime.agent_def["max_rounds"].as_u64().unwrap_or(super::super::MAX_ROUNDS as u64) as usize;

    // ── Step 2: Resolve tool list ─────────────────────────────────────────────
    let mut tool_names: Vec<String> = runtime.agent_def["tool_names"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    // Always strip think tool; native_think models handle reasoning internally.
    tool_names.retain(|t| t != "think");
    let use_skill_pass = runtime.agent_def["use_skill_pass"].as_bool().unwrap_or(false);
    tool_names = inject_required_tools(tool_names, runtime.needs_frontend(), use_skill_pass);

    let t_params = pre_llm_t0.elapsed();

    // ── Step 3: Session registration ─────────────────────────────────────────
    // Carry forward WorkingMemory and active skills from the previous run so that:
    // - Previous tool results remain citable ([cite:id] from prior messages is valid)
    // - Gmail/Calendar skills stay active for follow-up messages
    let tx = Arc::new(Transaction::new());
    {
        let mut sessions = runtime.agent_sessions.lock().await;
        if let Some(old) = sessions.get(runtime.conv_id.as_str()) {
            old.cancel.store(true, Ordering::Relaxed);
            if let Some(ref old_tx) = old.transaction {
                let _ = old_tx.cancel().await;
            }
            // Replace runtime's fresh WM with the carried-over one, then reset per-run state.
            runtime.working_memory = old.working_memory.clone();
            runtime.working_memory.start_new_run().await;
            // Seed runtime.active_skills from previous turn so build_agent_context
            // can detect carry-over before the skill pass fires.
            if let Some(ref skills) = old.active_skills {
                *runtime.active_skills.write().await = Some(skills.clone());
            }
        }
        sessions.insert((*runtime.conv_id).clone(), AgentSession {
            session_id:     Arc::clone(&runtime.session_id),
            conv_id:        Arc::clone(&runtime.conv_id),
            cancel:         Arc::clone(&runtime.cancel),
            answer_channel: Arc::clone(&runtime.answer_channel),
            transaction:    Some(Arc::clone(&tx)),
            working_memory: runtime.working_memory.clone(),
            active_skills:  None,
        });
    }

    let t_session = pre_llm_t0.elapsed();

    // ── Step 4: Emitter is pre-built in runtime ───────────────────────────────
    let emitter = &runtime.emitter;

    // ── Step 5+6: Pre-pass + context ─────────────────────────────────────────
    // Capture carry-over state BEFORE build_agent_context fires the skill pass.
    // had_active_skills = true → LLM already knows its next step, don't force tool use.
    let skills_are_cached = runtime.active_skills.read().await.is_some();
    let AgentContextResult { mem_facts_count } =
        runtime.build_agent_context(&input, activity_context.as_deref()).await;
    let t_context = pre_llm_t0.elapsed();

    // ── Step 7: Tool loop ─────────────────────────────────────────────────────
    let full_response = run_tool_loop(
        &runtime, &tool_names,
        enable_think, runtime.native_think, max_rounds, skills_are_cached,
        Arc::clone(&tx), &input,
        pre_llm_t0, t_params, t_session, t_context,
    ).await;

    // ── Step 8: Post-processing ───────────────────────────────────────────────
    let msgs_final = runtime.get_context_msgs(false).await;
    runtime.post_process(&msgs_final, &full_response, &input, mem_facts_count, session_t0.elapsed()).await;

    // Persist WorkingMemory and active skills back into the session for the next message.
    if !runtime.is_background() {
        let skills_snapshot = runtime.active_skills.read().await.clone();
        let mut sessions = runtime.agent_sessions.lock().await;
        if let Some(sess) = sessions.get_mut(runtime.conv_id.as_str()) {
            sess.working_memory = runtime.working_memory.clone();
            if skills_snapshot.is_some() {
                sess.active_skills = skills_snapshot;
            }
        }
    }

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

fn inject_required_tools(mut tool_names: Vec<String>, needs_frontend: bool, use_skill_pass: bool) -> Vec<String> {
    let mut required: Vec<&str> = vec![
        "get_session_state",
        "compress_context", "finish",
        "checkpoint", "clear_checkpoint",
        "save_agent_knowledge", "get_agent_knowledge",
        "batch_apply",
    ];
    // search_skills only when use_skill_pass=true: allows reactive mid-conversation
    // skill activation (e.g. multi-turn "好" after LLM asks "要整理為筆記嗎？").
    if use_skill_pass {
        required.push("search_skills");
    }
    for name in required {
        if !tool_names.contains(&name.to_string()) {
            tool_names.push(name.to_string());
        }
    }

    if needs_frontend {
        if !tool_names.contains(&"progress".to_string()) {
            tool_names.push("progress".to_string());
        }
    }

    let has_vault_tools = tool_names.iter().any(|t| VAULT_TOOL_MARKERS.contains(&t.as_str()));
    if has_vault_tools && !tool_names.contains(&"get_vault_changes".to_string()) {
        tool_names.push("get_vault_changes".to_string());
    }

    tool_names
}

/// Single LLM invocation — handles both streaming and non-streaming, with or without tools.
/// Returns `(full_text, tool_chunks, cite_invalid)`.
/// `cite_invalid` is true when a streaming round detected a fabricated citation ID.
async fn run_one_llm_round(
    runtime:     &HarnessRequestRuntime,
    msgs:        &[Value],
    tools:       Option<Value>,
    tool_choice: Value,
    tool_names:  &[String],
    emitter:     &super::super::harness::observability::emitter::ObservabilityEmitter,
) -> Result<(String, Vec<(String, String, String)>, bool), String> {
    use super::super::harness::tools::llm;
    let has_tools = tools.is_some();
    if runtime.streaming {
        let body = match tools {
            Some(tv) => json!({ "messages": msgs, "tools": tv, "tool_choice": tool_choice,
                               "stream": true, "temperature": 0.7, "max_tokens": 4096,
                               "cache_prompt": true }),
            None     => json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 4096,
                               "cache_prompt": true }),
        };
        let wm = if has_tools { Some(&runtime.working_memory) } else { None };
        let tnames: &[String] = if has_tools { tool_names } else { &[] };
        llm::stream_llm_round(
            &runtime.client, &runtime.llm_url, body, emitter, &runtime.cancel, wm, tnames,
            runtime.native_think,
        ).await.map(|(t, _, chunks, ci)| (t, chunks, ci))
    } else {
        llm::call_llm_once(
            &runtime.client, &runtime.llm_url, msgs, tools, &runtime.cancel,
        ).await.map(|(t, chunks)| (t, chunks, false))
    }
}

async fn run_tool_loop(
    runtime:          &HarnessRequestRuntime,
    tool_names:       &[String],
    enable_think:     bool,
    native_think:     bool,
    max_rounds:       usize,
    skills_are_cached: bool,
    tx:               Arc<Transaction>,
    input:            &str,
    pre_llm_t0:       std::time::Instant,
    t_params:         std::time::Duration,
    t_session:        std::time::Duration,
    t_context:        std::time::Duration,
) -> String {
    let emitter = &runtime.emitter;
    let mut full_response = String::new();
    // Transient corrections: appended to each round's snapshot but never written to the
    // context buffer (and therefore never stored in DB or seen by memory agent).
    let mut pending_corrections: Vec<Value> = Vec::new();

    // inject_required_tools always returns non-empty (search_skills etc. are always added),
    // so tool_names is guaranteed non-empty here and the else branch is never taken.
    let mut tool_names: Vec<String> = tool_names.to_vec();

    if !tool_names.is_empty() {
        let mut tools_schema = build_tools_schema(&tool_names);
        let mut tools_value  = if tools_schema.is_empty() { None } else { Some(json!(tools_schema)) };
        // When native_think=true the model produces <think>...</think> blocks on its own;
        // skip injecting the think tool regardless of the agent's enable_think setting.
        let think_schema = if enable_think && !native_think {
            Some(build_tools_schema(&["think".to_string()]))
        } else {
            None
        };

        // ── Sync active skills from session → tool schema ────────────────────
        // Read agent_sessions[conv_id].active_skills and:
        //   1. Add any skill tools not yet in the live schema.
        //   2. Push system_injection to pending_corrections (once, deduped).
        //   3. Emit agent:skills_activated with ALL currently active titles so the
        //      frontend can replace its display with the current state.
        // Called before round 0 (pre-pass) and after each dispatch (reactive search_skills).
        macro_rules! sync_active_skills {
            () => {{
                let (all_titles, all_tools, injection) = {
                    runtime.active_skills.read().await
                        .as_ref()
                        .map(|s| (s.skill_titles.clone(), s.skill_tool_names.clone(), s.system_injection.clone()))
                        .unwrap_or_default()
                };
                tracing::info!(
                    "[sync_active_skills] titles={:?} tools={:?} injection_len={}",
                    all_titles, all_tools, injection.len()
                );
                // Merge new tools into schema.
                let mut added = false;
                for t in &all_tools {
                    if !tool_names.contains(t) {
                        tool_names.push(t.clone());
                        added = true;
                    }
                }
                if added {
                    tools_schema = build_tools_schema(&tool_names);
                    tools_value  = if tools_schema.is_empty() { None } else { Some(json!(tools_schema)) };
                }
                // Push skill directive (deduplicated by content).
                if !injection.is_empty() {
                    let already = pending_corrections.iter()
                        .any(|m| m["content"].as_str() == Some(injection.as_str()));
                    if !already {
                        pending_corrections.push(json!({ "role": "user", "content": injection }));
                    }
                }
                // Always emit current active titles so frontend replaces its display.
                // record_skill_activations handles both state recording and SSE emit.
                if !all_titles.is_empty() {
                    emitter.record_skill_activations(&all_titles);
                }
            }};
        }

        // Sync before round 0 (pre-pass results already written to agent_sessions).
        sync_active_skills!();
        let skills_are_cached = skills_are_cached;
        let t_sync = pre_llm_t0.elapsed();
        tracing::info!(
            "[pre_llm] params={}ms session={}ms context={}ms sync={}ms total={}ms",
            t_params.as_millis(),
            (t_session - t_params).as_millis(),
            (t_context - t_session).as_millis(),
            (t_sync - t_context).as_millis(),
            t_sync.as_millis(),
        );

        // Limit cite-correction retries to avoid infinite loops when the LLM
        // cannot learn the format within a few attempts.
        const MAX_CITE_CORRECTIONS: usize = 2;
        let mut cite_correction_count = 0usize;

        for round in 0..max_rounds.min(super::super::MAX_ROUNDS) {
            if runtime.cancel.load(Ordering::Relaxed) { break; }
            emitter.increment_round();

            // In the first content round (round 0 without think, round 1 after think),
            // force tool_choice="required" when chain skills are active so the model
            // cannot skip tool calls and fabricate an answer.
            let is_first_content_round = if enable_think { round == 1 } else { round == 0 };
            // Only force tool use when skills are freshly activated (not carry-over from
            // a previous message). Forcing on cached-skill turns causes the LLM to restart
            // the skill chain from step 1 (e.g. re-calling list_emails) even when step 1
            // was already done.
            let (has_active, skill_has_tools) = {
                let guard = runtime.active_skills.read().await;
                (guard.is_some(), guard.as_ref().map(|s| !s.skill_tool_names.is_empty()).unwrap_or(false))
            };
            let force_tool_use = is_first_content_round
                && !tool_names.is_empty()
                && !skills_are_cached
                && has_active
                && skill_has_tools;
            tracing::debug!(
                "[agent] round={} is_first_content={} skills_cached={} has_active={} force_tool_use={}",
                round, is_first_content_round, skills_are_cached, has_active, force_tool_use
            );

            let (round_tools, round_choice) = if round == 0 {
                if let Some(ref ts) = think_schema {
                    (Some(json!(ts)), json!({ "type": "function", "function": { "name": "think" } }))
                } else {
                    let choice = if force_tool_use { json!("required") } else { json!("auto") };
                    (tools_value.clone(), choice)
                }
            } else {
                let choice = if force_tool_use { json!("required") } else { json!("auto") };
                (tools_value.clone(), choice)
            };

            let mut msgs_snapshot = runtime.get_context_msgs(true).await;
            // Append transient corrections from previous rounds (not in context buffer).
            msgs_snapshot.extend(pending_corrections.clone());
            // On the first content round when skills are active (whether freshly activated
            // or cached), inject a transient directive. For freshly-activated skills just say
            // "call immediately". For cached skills, also list already-executed tools from WM
            // so the LLM knows which steps are done and doesn't re-run them from step 1.
            // When skills are cached (carry-over from previous message) and tools have
            // already run this session, inject a hint listing completed tools so the LLM
            // knows which step to continue from rather than restarting from step 1.
            let has_skills = runtime.active_skills.read().await.is_some();
            if is_first_content_round && has_skills && skills_are_cached && !runtime.working_memory.is_empty().await {
                let pairs = runtime.working_memory.cite_id_tool_pairs().await;
                let mut seen = std::collections::HashSet::new();
                let names: Vec<&str> = pairs.iter()
                    .filter_map(|(_, n)| seen.insert(n.as_str()).then_some(n.as_str()))
                    .collect();
                let hint = match runtime.locale {
                    super::super::harness::prompt::Locale::En => format!(
                        "Tools already completed this session: {}. \
                         Do NOT re-run them. Call the NEXT required tool directly.",
                        names.join(", ")
                    ),
                    _ => format!(
                        "本 session 已完成的工具：{}。禁止重複呼叫這些工具。直接呼叫下一個所需工具。",
                        names.join("、")
                    ),
                };
                msgs_snapshot.push(json!({ "role": "user", "content": hint }));
            }

            let llm_t0 = std::time::Instant::now();
            let (text, tool_chunks, cite_invalid) = match run_one_llm_round(
                runtime, &msgs_snapshot, round_tools, round_choice, &tool_names, emitter,
            ).await {
                Ok(r) => r,
                Err(e) => { tracing::warn!("[agent/tools] llm error: {}", e); break; }
            };
            emitter.record_llm_latency(llm_t0.elapsed().as_millis() as u64);

            if !text.is_empty() {
                if native_think && text.contains("<think>") {
                    // Streaming already routed think tokens to llm:think_token and answer
                    // tokens to llm:token. Strip think blocks here only for DB storage so
                    // the LLM's context window doesn't accumulate chain-of-thought noise.
                    full_response = strip_think_blocks(&text);
                } else {
                    full_response = text.clone();
                }
            }

            // Think round (round=0 with enable_think): if LLM outputs text instead of
            // calling the `think` tool, skip it and continue to the content round.
            // Small/quantized models often ignore tool_choice:{name:think} and just answer.
            // Treating this text as a final answer would bypass the content round entirely.
            let is_think_round = enable_think && round == 0;
            if is_think_round && tool_chunks.is_empty() && !text.trim().is_empty() {
                tracing::warn!("[agent] think round: LLM output text instead of think tool — skipping to content round");
                // Push as assistant context so the content round sees what the model said.
                runtime.push_msg(json!({ "role": "assistant", "content": text })).await;
                emitter.emit("agent:clear_stream".to_string(), json!({}));
                continue;
            }

            // If this round required tool use (force_tool_use=true) but the LLM ignored
            // tool_choice:"required" and produced plain text instead, push the text as
            // assistant context and inject a correction to force the tool call next round.
            // Quantized/small models frequently ignore tool_choice even when set to "required".
            // Limit retries to avoid looping forever.
            if force_tool_use && tool_chunks.is_empty() && !text.trim().is_empty() {
                tracing::warn!("[agent] LLM ignored tool_choice:required — injecting tool-call correction");
                let msg = match runtime.locale {
                    super::super::harness::prompt::Locale::En => format!(
                        "[System] You must call a tool now to fulfil the user's request: \"{}\". \
                         Do not output any text. Call the required tool immediately.",
                        input
                    ),
                    _ => format!(
                        "[系統] 你必須立即呼叫工具來完成使用者的請求：「{}」。\
                         不要輸出任何文字，直接使用工具呼叫格式。",
                        input
                    ),
                };
                pending_corrections.clear();
                pending_corrections.push(json!({ "role": "assistant", "content": text }));
                pending_corrections.push(json!({ "role": "user", "content": msg }));
                emitter.emit("agent:clear_stream".to_string(), json!({}));
                // continue to next round — force_tool_use will be false (not first content round)
                // but skills are still active and tool_choice will be "auto"; the correction
                // message is the primary driver
                continue;
            }

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
                    Err(e) => {
                        tracing::warn!("[agent/tools] dispatcher error: {}", e);
                        let error_content = format!("[工具執行失敗] {}", e);
                        let now = chrono::Utc::now().timestamp();
                        // Record each failed tool in WorkingMemory so the LLM has a valid
                        // cite_id to reference when explaining the error to the user.
                        // Without this, cite enforcement would fire because
                        // current_run_has_results() may already be true (from a sibling tool
                        // that succeeded earlier in the same round), but the LLM cannot cite
                        // the error result because its ID was never registered.
                        for (tc_id, tc_name, tc_args) in &tool_chunks {
                            let args_v = serde_json::from_str::<Value>(tc_args).unwrap_or(json!({}));
                            runtime.working_memory.record(
                                tc_id.clone(), tc_name.clone(), args_v,
                                json!({ "error": error_content.clone() }),
                                now, 0, GuardOutcome::Exempt,
                            ).await;
                        }
                        let error_msgs: Vec<Value> = tool_chunks.iter().map(|(tc_id, tc_name, _)| json!({
                            "role": "tool",
                            "tool_call_id": tc_id,
                            "name": tc_name,
                            "content": error_content.clone(),
                        })).collect();
                        runtime.extend_msgs_guarded(error_msgs).await;
                        continue;
                    }
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

                // Re-sync after dispatch: picks up any new skills written by
                // handle_search_skills during this round (reactive activation).
                sync_active_skills!();
            } else if cite_invalid && cite_correction_count < MAX_CITE_CORRECTIONS {
                cite_correction_count += 1;
                // LLM fabricated a citation ID or produced no cite tag at all.
                // Push its reply as assistant message and inject a correction that
                // explains *what* a cite ID is and lists available IDs with their
                // corresponding tool names, so the LLM can connect ID → result.
                let current_run_has_results = runtime.working_memory.current_run_has_results().await;
                let all_pairs = runtime.working_memory.cite_id_tool_pairs().await;
                let current_pairs = runtime.working_memory.current_run_cite_pairs().await;
                let correction = if all_pairs.is_empty() {
                    // No tools called in this session at all.
                    match runtime.locale {
                        super::super::harness::prompt::Locale::En =>
                            "[System] You referenced a tool result, but no tools have been called \
                             yet in this session. You must call the appropriate tool first and wait \
                             for its result before answering. Do not invent data or citation IDs. \
                             Please call the tool now.".to_string(),
                        _ =>
                            "[系統] 你的回覆引用了工具結果，但本 session 尚未執行任何工具。\
                             請先呼叫對應工具，等待工具回傳真實結果後再回覆。\
                             不可自行捏造資料。請立即呼叫工具。".to_string(),
                    }
                } else if !current_run_has_results {
                    // Tools were called in a previous round but NOT this round.
                    // The LLM is trying to answer from carry-over context without fetching
                    // the data the user is actually asking for (e.g. fabricating email body
                    // without calling read_email). Tell it to call the appropriate tool.
                    match runtime.locale {
                        super::super::harness::prompt::Locale::En =>
                            "[System] You have not called any tool this round. \
                             If the user's request requires fetching new data (e.g. reading a specific \
                             email, note, or external resource), you MUST call the appropriate tool \
                             first and wait for the real result before answering. \
                             Do not invent or paraphrase content you have not fetched. \
                             Please call the required tool now.".to_string(),
                        _ =>
                            "[系統] 你本輪沒有呼叫任何工具。\
                             若使用者的需求需要取得新資料（如：閱讀特定郵件、筆記或外部資源），\
                             你必須先呼叫對應工具，等待工具回傳真實結果後才能回覆。\
                             不可自行捏造或推測尚未查詢到的內容。請立即呼叫所需工具。".to_string(),
                    }
                } else {
                    // Tools were called this round — the LLM cited a wrong/invalid ID.
                    // Only show IDs from the current run to avoid confusion with stale carry-over.
                    let pairs = if !current_pairs.is_empty() { &current_pairs } else { &all_pairs };
                    let mapping: String = pairs.iter()
                        .map(|(id, name)| format!("  [cite:{}]  ← {} 的結果", id, name))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let mapping_en: String = pairs.iter()
                        .map(|(id, name)| format!("  [cite:{}]  ← result of {}", id, name))
                        .collect::<Vec<_>>()
                        .join("\n");
                    match runtime.locale {
                        super::super::harness::prompt::Locale::En => format!(
                            "[System] Your answer contains an invalid citation. \
                             [cite:ID] must reference an actual tool result from this session — \
                             the ID is not arbitrary, it identifies a specific result.\n\
                             Available citations from this round:\n{}\n\
                             Please rewrite your answer using one of the IDs above. \
                             If your answer does not rely on any tool result, write [cite:none] instead.",
                            mapping_en
                        ),
                        _ => format!(
                            "[系統] 你的回覆中使用了無效的 cite ID。\
                             [cite:ID] 必須對應本 session 中實際執行過的工具結果，ID 不能自行發明。\n\
                             本輪可用的引用：\n{}\n\
                             請根據上方結果重新回覆，並在回覆開頭加上對應的 [cite:ID]。\
                             若你的回覆不依賴任何工具結果，請使用 [cite:none]。",
                            mapping
                        ),
                    }
                };
                // Correction is transient: appended to the next round's snapshot but never
                // written to context buffer or DB, so memory agent stays clean.
                let correction_with_input = match runtime.locale {
                    super::super::harness::prompt::Locale::En =>
                        format!("{}\n\n(User's original request: \"{}\")", correction, input),
                    _ =>
                        format!("{}\n\n（使用者原始請求：「{}」）", correction, input),
                };
                pending_corrections.clear();
                if !text.is_empty() {
                    pending_corrections.push(json!({ "role": "assistant", "content": text }));
                }
                pending_corrections.push(json!({ "role": "user", "content": correction_with_input }));
                // Signal frontend to clear the streaming buffer before correction round
                emitter.emit("agent:cite_correction_start".to_string(), json!({}));
                // continue to next round for correction
            } else {
                // Final answer round.
                // Only emit cite status when cite validation was actually applicable —
                // i.e. tools ran this round, OR WM has carry-over data and the response
                // is substantive (>120 chars). This mirrors the `cite_required` check in
                // stream_llm_round so that short replies without tool results don't
                // show a badge (which would have no details to expand and misleads the user).
                let cite_run_has = runtime.working_memory.current_run_has_results().await;
                let cite_wm_any  = !runtime.working_memory.is_empty().await;
                let cite_applicable = cite_run_has
                    || (cite_wm_any && full_response.chars().count() > 120);
                if cite_applicable {
                    emitter.emit("agent:cite_status".to_string(), json!({ "passed": !cite_invalid }));
                }
                // runtime.active_skills persists until turn end where it is written
                // back to agent_sessions — no early save/clear needed here.
                pending_corrections.clear();
                break;
            }
        }
    } else {
        emitter.increment_round();
        let msgs_snapshot = runtime.get_context_msgs(false).await;
        let llm_t0 = std::time::Instant::now();
        match run_one_llm_round(runtime, &msgs_snapshot, None, json!("auto"), &[], emitter).await {
            Ok((text, _, _)) => {
                if native_think && text.contains("<think>") {
                    // Streaming already routed think tokens to llm:think_token; strip for DB storage.
                    full_response = strip_think_blocks(&text);
                } else {
                    full_response = text;
                }
            }
            Err(e) => { tracing::warn!("[agent/pure] llm error: {}", e); }
        }
        emitter.record_llm_latency(llm_t0.elapsed().as_millis() as u64);
    }

    full_response
}

/// Remove all `<think>…</think>` blocks from a native-think model's output.
/// Leading/trailing whitespace is trimmed so the remaining answer is clean.
fn strip_think_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        if let Some(end) = rest[start..].find("</think>") {
            rest = &rest[start + end + "</think>".len()..];
        } else {
            // Unclosed tag — drop everything from here.
            rest = "";
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}
