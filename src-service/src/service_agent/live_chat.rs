use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use serde_json::{json, Value};
use crate::api_state::ApiState;
use crate::state::AgentSession;

/// Entry point for invoke_live_chat.
///
/// 3-round fixed flow:
///   Round 0: think + search_skills → get skill tool names
///   Round 1: think + skill tools   → execute tools, gather data
///   Round 2: live_respond only     → output final oral answer
///
/// Emits:
///   llm:token        → streaming tokens (rounds 0-1)
///   live_chat:action → {speech, action, content?, error?}  (terminal event)
/// Returns the final speech string (empty if cancelled or error).
pub async fn run_live_chat_agent(
    state: ApiState,
    session_id: String,
    input: String,
    language: Option<String>,
    note_context: Option<String>,
    activity_context: Option<String>,
    vault_id: String,
    account_id: String,
    vault_path: String,
    conversation_id: String,
) -> String {
    // Register cancel flag
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut sessions = state.daemon.agent_sessions.lock().await;
        sessions.insert(conversation_id.clone(), AgentSession {
            session_id: session_id.clone(),
            cancel: Arc::clone(&cancel),
            transaction: None,
            conversation_id: conversation_id.clone(),
        });
    }

    let llm_url = match state.daemon.llm_url.read().await.clone() {
        Some(u) => u,
        None => {
            state.daemon.emit("live_chat:action", json!({
                "speech": "抱歉，語言模型尚未就緒。", "action": "show_error",
            }));
            return String::new();
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_default();
    let embedding_url = state.daemon.embedding_url.read().await.clone();

    // Load messages from DB and append current user input
    let history = super::helpers::load_messages_db(&state.db, &conversation_id).await;
    let mut base_msgs: Vec<Value> = history;
    base_msgs.push(json!({"role": "user", "content": input}));

    // Assemble system prompt (same logic as Tauri invoke_live_chat)
    let lang_hint = match language.as_deref().unwrap_or("zh-TW") {
        "en" => "Reply in English.",
        "ja" => "日本語で返答してください。",
        "de" => "Bitte auf Deutsch antworten.",
        "ko" => "한국어로 답변해 주세요.",
        _ => "請用繁體中文口語回答。",
    };
    let note_ctx_hint = if let Some(ref nc) = note_context {
        format!("\n\n[當前開啟的筆記]\n{}", nc)
    } else {
        String::new()
    };
    let activity_ctx_hint = if let Some(ref ac) = activity_context {
        format!("\n\n[使用者活動紀錄]\n{}", ac)
    } else {
        String::new()
    };

    // Prefetch memory facts
    let memory_ctx_hint = if !vault_id.is_empty() && !account_id.is_empty() {
        let now = chrono::Utc::now().timestamp();
        // Build keywords from input (simple approach: use first ~120 chars as search)
        let kw_input: String = input.chars().take(120).collect();
        let keywords: Vec<String> = if kw_input.is_empty() { vec![] } else { vec![kw_input.clone()] };

        // Try semantic search if embedding_url available
        let facts: Vec<Value> = if let Some(ref emb_url) = embedding_url {
            // Semantic search via embedding
            if let Some(query_vec) = crate::embedder::embed_text(&client, &Some(emb_url.clone()), &kw_input).await {
                #[derive(serde::Deserialize)]
                struct FactRow { fact_id: String, content: String, category: String, embedding: Option<Vec<f32>> }
                let mut r = state.db
                    .query("SELECT fact_id, content, category, embedding FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now ORDER BY created_at DESC LIMIT 20")
                    .bind(("vid", vault_id.clone()))
                    .bind(("aid", account_id.clone()))
                    .bind(("now", now))
                    .await
                    .ok();
                if let Some(ref mut resp) = r {
                    let rows: Vec<FactRow> = resp.take(0).unwrap_or_default();
                    let mut scored: Vec<(f32, Value)> = rows.into_iter().filter_map(|row| {
                        let sim = row.embedding.as_ref().map(|e| super::helpers::cosine_sim(e, &query_vec)).unwrap_or(0.0);
                        Some((sim, json!({"fact_id": row.fact_id, "content": row.content, "category": row.category})))
                    }).collect();
                    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                    scored.into_iter().take(6).map(|(_, v)| v).collect()
                } else { vec![] }
            } else {
                // Fallback to keyword search
                super::helpers::vault_query_memory_with_limit(&state.db, &vault_id, &account_id, &keywords, 6).await
            }
        } else {
            super::helpers::vault_query_memory_with_limit(&state.db, &vault_id, &account_id, &keywords, 6).await
        };

        if !facts.is_empty() {
            let node_ids: Vec<String> = facts.iter()
                .filter_map(|r| r["fact_id"].as_str())
                .map(|fid| format!("memory:{}:{}", vault_id, fid))
                .collect();
            if !node_ids.is_empty() {
                state.daemon.emit("memory:prefetched", json!({
                    "node_ids": node_ids,
                    "source": "chat",
                }));
            }
            let lines: Vec<String> = facts.iter().filter_map(|r| {
                let cat = r["category"].as_str()?;
                let content = r["content"].as_str()?;
                Some(format!("[{}] {}", cat, content))
            }).collect();
            format!("\n\n[你對使用者的了解]\n{}", lines.join("\n"))
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let system = format!(
        "你是一個語音助理，使用者透過語音與你對話。{}\
你會分三輪被呼叫：第一輪呼叫 think + search_skills、第二輪呼叫 think + 執行工具取得資料、第三輪呼叫 live_respond 輸出最終口語回覆。\
think 規則：每次呼叫工具之前，必須先呼叫 think 輸出一句內心獨白（10字以內），描述你正在想什麼或接下來要做什麼。例如：「這讓我想起...」「讓我查一下...」「嗯，可能是...」。\
live_respond 規則：\
- speech：TTS 朗讀文字，口語化繁體中文，2 句以內，不含 Markdown 或符號。若有搜尋結果，speech 說「已為您找到以下資訊」即可。\
- content：若有查到網頁或筆記內容，把完整摘要或重點放在此欄位供畫面顯示（可含換行，無需口語化）。若無額外資料則省略。\
- action：show_results（有資料需展示）/ open_note（開啟筆記）/ open_tab（切換頁籤）/ show_error（錯誤）/ none（只 TTS）。{}{}{}",
        lang_hint, note_ctx_hint, activity_ctx_hint, memory_ctx_hint
    );

    // Apply sliding window (8000 chars) and prepend system
    let hist: Vec<Value> = base_msgs.iter()
        .filter(|m| m["role"].as_str() != Some("system"))
        .cloned().collect();
    const MAX_HISTORY_CHARS: usize = 8000;
    let total: usize = hist.iter().map(|m| m["content"].as_str().unwrap_or("").len()).sum();
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
    let mut msgs: Vec<Value> = std::iter::once(json!({"role": "system", "content": system}))
        .chain(trimmed.into_iter())
        .collect();

    let mut final_speech = String::new();
    let mut live_action: Option<Value> = None;
    let mut skill_tool_names: Vec<String> = Vec::new();

    // Emit helper for error action
    let emit_error = |state: &ApiState, speech: &str, detail: String| {
        state.daemon.emit("live_chat:action", json!({
            "speech": speech,
            "action": "show_error",
            "error": detail,
        }));
    };

    'tool_loop: for round in 0..3usize {
        if cancel.load(Ordering::Relaxed) { break; }

        if round == 1 && skill_tool_names.is_empty() {
            // search_skills returned nothing → skip to live_respond
            continue;
        }

        // Build tool names for this round
        let round_tool_names: Vec<String> = match round {
            0 => vec!["think".to_string(), "search_skills".to_string()],
            1 => {
                let mut n = vec!["think".to_string()];
                n.extend(skill_tool_names.iter().cloned());
                n
            }
            _ => vec!["live_respond".to_string()],
        };

        let tools_schema = super::vault_tools::build_tools_schema_interactive(&round_tool_names);
        let body = if tools_schema.is_empty() {
            json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 512 })
        } else {
            json!({
                "messages": msgs,
                "tools": tools_schema,
                "tool_choice": "required",
                "stream": true,
                "temperature": 0.7,
                "max_tokens": 512,
            })
        };

        let (text, finish_reason, tool_chunks) = match super::vault_tools::stream_llm_round(
            &client, &llm_url, body, &state, &session_id, &cancel,
        ).await {
            Ok(r) => r,
            Err(e) => {
                emit_error(&state, "抱歉，語言模型回應失敗，請稍後再試。", format!("LLM error: {e}"));
                return String::new();
            }
        };

        // Round 2: expect live_respond tool call or fallback to plain text
        if round == 2 {
            if let Some(tc) = tool_chunks.iter().find(|(_, n, _)| n == "live_respond") {
                let args: Value = serde_json::from_str(&tc.2).unwrap_or(json!({}));
                final_speech = args["speech"].as_str().unwrap_or("").to_string();
                live_action = Some(args);
            } else if !text.is_empty() {
                final_speech = text.clone();
                live_action = Some(json!({ "speech": final_speech, "action": "none" }));
            }
            break 'tool_loop;
        }

        if tool_chunks.is_empty() { continue; }

        // Build assistant message with tool_calls
        let tc_json: Vec<Value> = tool_chunks.iter().map(|tc| json!({
            "id": tc.0, "type": "function",
            "function": { "name": tc.1, "arguments": tc.2 },
        })).collect();
        msgs.push(json!({ "role": "assistant", "content": Value::Null, "tool_calls": tc_json }));
        let _ = (text, finish_reason); // suppress unused warnings

        // Execute each tool call
        for (tc_id, tc_name, tc_args_str) in &tool_chunks {
            let args: Value = serde_json::from_str(tc_args_str).unwrap_or(json!({}));

            // plan_announce / think: auto-confirm without execution
            if matches!(tc_name.as_str(), "plan_announce" | "think") {
                msgs.push(json!({
                    "role": "tool", "tool_call_id": tc_id,
                    "content": "✅ 已自動確認，繼續執行",
                }));
                continue;
            }

            state.daemon.emit("live_chat:tool_call", json!({
                "session_id": session_id,
                "display": tc_name,
            }));

            let result = super::vault_tools::dispatch_interactive_tool(
                &client, &llm_url, &state.db,
                &vault_id, &account_id, &vault_path, &embedding_url,
                tc_name, &args,
            ).await;

            let result_str = match result {
                Ok((v, _rollback)) => {
                    // Round 0: search_skills → extract skill tool names
                    if tc_name == "search_skills" {
                        if let Some(arr) = v.as_array() {
                            // v is Vec<{title, behavior, required_tools}>
                            skill_tool_names = arr.iter()
                                .flat_map(|item| {
                                    item["required_tools"].as_array()
                                        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>())
                                        .unwrap_or_default()
                                })
                                .collect();
                        }
                        serde_json::to_string(&v).unwrap_or_default()
                    } else {
                        // Truncate large results
                        let s = serde_json::to_string(&v).unwrap_or_default();
                        if s.chars().count() > 2000 {
                            format!("{}…（已截斷）", s.chars().take(2000).collect::<String>())
                        } else {
                            s
                        }
                    }
                }
                Err(ref e) => format!("ERROR: {}", e),
            };

            msgs.push(json!({
                "role": "tool", "tool_call_id": tc_id,
                "content": result_str,
            }));
        }
    }

    // Save conversation to DB
    let mut to_save: Vec<Value> = base_msgs.iter()
        .filter(|m| m["role"].as_str() != Some("system"))
        .cloned()
        .collect();
    if !final_speech.is_empty() {
        to_save.push(json!({ "role": "assistant", "content": final_speech }));
    }
    let now = chrono::Utc::now().timestamp();
    let _ = state.db
        .query("UPDATE conversations SET messages_json = $msgs, updated_at = $now WHERE record::id(id) = $cid")
        .bind(("msgs", serde_json::to_string(&to_save).unwrap_or_else(|_| "[]".to_string())))
        .bind(("now", now))
        .bind(("cid", conversation_id.clone()))
        .await;

    // Emit live_chat:action or fallback
    if let Some(action_args) = live_action {
        state.daemon.emit("live_chat:action", action_args);
    } else if !cancel.load(Ordering::Relaxed) {
        emit_error(&state, "抱歉，我沒有得到有效的回覆，請再試一次。", "未能取得有效回應".to_string());
    }

    final_speech
}
