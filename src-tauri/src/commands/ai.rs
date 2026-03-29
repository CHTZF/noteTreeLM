use crate::{error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};

// ─── Re-exports from sub-modules ──────────────────────────────────────────────
pub use super::server::{
    get_embedding, warmup_llama_server,
    stop_llama_server, get_llama_server_status, start_llama_server, restart_llama_server,
    warmup_embedding_server, get_embedding_server_status, check_embedding_endpoint,
    start_embedding_server, stop_embedding_server, restart_embedding_server,
};
pub(crate) use super::server::ensure_server_running;
pub use super::external_ai::process_with_llm;
pub use super::memory::{
    query_memory,
    rate_response, get_conversation_ratings,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}
// Agent factory — see runtime/agent_factory.rs
pub(crate) use crate::runtime::agent_factory::{generate_agent_spec, extract_cjk_keywords};

// LLM engine — see runtime/llm_engine.rs
pub use crate::runtime::llm_engine::compute_centroid;
pub(crate) use crate::runtime::llm_engine::{StreamResult, send_streaming_request};

/// 取消正在進行的 Agent 串流
#[tauri::command]
pub async fn cancel_agent(state: State<'_, AppState>) -> Result<(), AppError> {
    let session_id = state.agent_session.lock().await.clone();
    let vault_id = state.get_vault_id().await.unwrap_or_default();
    let auth_token = state.get_auth_token().await;
    let tok = if auth_token.is_empty() { None } else { Some(auth_token.as_str()) };

    if let Some(sid) = session_id {
        let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
            &state.http_client,
            &format!("/vaults/{}/agent/cancel", urlencoding::encode(&vault_id)),
            &serde_json::json!({ "session_id": sid }),
            tok,
        ).await;
    }
    // Keep local flag for the no-vault (live chat) path
    state.agent_cancel.store(true, Ordering::Relaxed);
    if let Some(tx) = state.write_confirm_tx.lock().await.take() {
        let _ = tx.send(false);
    }
    Ok(())
}

/// 前端確認/拒絕寫入工具
#[tauri::command]
pub async fn confirm_write_tool(
    state: State<'_, AppState>,
    approved: bool,
) -> Result<(), AppError> {
    let session_id = state.agent_session.lock().await.clone();
    let vault_id = state.get_vault_id().await.unwrap_or_default();
    let auth_token = state.get_auth_token().await;
    let tok = if auth_token.is_empty() { None } else { Some(auth_token.as_str()) };

    if let Some(sid) = session_id {
        let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
            &state.http_client,
            &format!("/vaults/{}/agent/confirm", urlencoding::encode(&vault_id)),
            &serde_json::json!({ "session_id": sid, "approved": approved }),
            tok,
        ).await;
    }
    // Keep local fallback for live chat path
    let tx = state.write_confirm_tx.lock().await.take();
    if let Some(tx) = tx {
        let _ = tx.send(approved);
    }
    Ok(())
}

/// 帶意圖分類的 Agent 串流
/// 透過 runtime::Agent 執行：意圖分類 → 取消/確認/多輪 LLM+工具 loop
/// 每輪工具呼叫透過 ToolRegistry + Transaction 執行（支援 rollback）
///
/// conversation_id 存在時：後端從 DB 載入 messages，不使用前端傳入的 messages；
/// 回應完成後自動將完整對話（含 LLM 回覆）存回 DB。
#[tauri::command]
pub async fn invoke_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    input: String,
    messages: Vec<ChatMessage>,
    system: Option<String>,
    use_tools: Option<bool>,
    conversation_id: Option<String>,
    activity_context: Option<String>, // 使用者活動紀錄，由前端提供
) -> Result<String, AppError> {
    use crate::commands::conversation::{load_messages, save_messages, maybe_set_title};
    use crate::runtime::intent_classifier::{Intent, IntentClassifier};
    use crate::runtime::types::{EmitEventFn, LlmFn, LlmRound};

    // 1. 確保 llama-server 運行，取得 base_url
    let base_url = ensure_server_running(state.inner(), &app).await?;
    let client = state.http_client.clone();

    // 2. Vault 資訊
    let vault_path = state.get_vault_path().await;
    let vault_id_opt = state.get_vault_id().await.ok();
    let auth_token = state.get_auth_token().await;
    let tok = if auth_token.is_empty() { None } else { Some(auth_token.as_str()) };

    // 3. 組裝 messages_json
    //    - conversation_id 存在：從 DB 載入歷史，追加當前 user 訊息
    //    - 否則：使用前端傳入的 messages（向下相容）
    let mut messages_json: Vec<serde_json::Value> = if let Some(ref conv_id) = conversation_id {
        let mut db_msgs = load_messages(&state.http_client, tok, conv_id).await?;
        let arr = db_msgs.as_array_mut()
            .ok_or_else(|| AppError::AI("messages_json 格式錯誤".into()))?;
        // 追加當前 user 訊息（input）
        arr.push(serde_json::json!({"role": "user", "content": input}));
        arr.clone()
    } else {
        let mut v: Vec<serde_json::Value> = Vec::new();
        for msg in &messages {
            v.push(serde_json::json!({"role": msg.role, "content": msg.content}));
        }
        v
    };

    // 若有 system prompt，插入最前面（conversation_id 模式下 system 每次由前端提供）
    if let Some(ref sys) = system {
        let sys_with_activity = if let Some(ref ac) = activity_context {
            format!("{}\n\n[使用者活動紀錄]\n{}", sys, ac)
        } else {
            sys.clone()
        };
        // 若第一條已是 system，覆蓋它；否則插入
        if messages_json.first().and_then(|m| m["role"].as_str()) == Some("system") {
            messages_json[0] = serde_json::json!({"role": "system", "content": sys_with_activity});
        } else {
            messages_json.insert(0, serde_json::json!({"role": "system", "content": sys_with_activity}));
        }
    }

    let skill_emb_url: Option<String> = Some(base_url.clone());

    // 4. llm_fn：執行一輪 LLM 串流，返回 LlmRound（用於 no-vault 純對話路徑）
    let client_fn = client.clone();
    let base_fn = base_url.clone();
    let app_fn = app.clone();
    let llm_fn: LlmFn = Arc::new(move |msgs, tools_opt, cancel| {
        let client = client_fn.clone();
        let base = base_fn.clone();
        let app = app_fn.clone();
        Box::pin(async move {
            let body = if let Some(tools) = tools_opt {
                serde_json::json!({
                    "messages": msgs,
                    "tools": tools,
                    "tool_choice": "auto",
                    "max_tokens": 2048,
                    "temperature": 0.7,
                    "stream": true,
                })
            } else {
                serde_json::json!({
                    "messages": msgs,
                    "max_tokens": 2048,
                    "temperature": 0.7,
                    "stream": true,
                })
            };
            let result = send_streaming_request(&client, &base, body, &app, cancel)
                .await
                .map_err(|e| e.to_string())?;
            let tool_calls = detect_tool_calls(&result);
            Ok(LlmRound { full_text: result.full_text, tool_calls })
        })
    });

    // 5. emit_fn：通用事件發送（包裝 AppHandle::emit）
    let app_emit = app.clone();
    let emit_fn: EmitEventFn = Arc::new(move |event: String, payload: serde_json::Value| {
        let _ = app_emit.emit(&event, payload);
    });

    // 7b. embed_fn：呼叫 llama-server /embedding，取得向量
    let client_emb = client.clone();
    let base_emb = base_url.clone();
    let embed_fn: crate::runtime::types::EmbedFn = Arc::new(move |text: String| {
        let client = client_emb.clone();
        let base = base_emb.clone();
        Box::pin(async move {
            get_embedding(&client, &base, &text).await
        })
    });

    // 8. Pending plan check（原 Agent::run 邏輯）
    // 9. Pending plan check（原 Agent::run 邏輯，移至此處統一處理）
    if let Some(ref conv_id) = conversation_id {
        use crate::commands::conversation::{load_pending_plan, delete_pending_plan};
        if let Ok(Some(pending)) = load_pending_plan(&state.http_client, tok, conv_id).await {
            let age = chrono::Utc::now().timestamp() - pending.created_at;
            let _ = delete_pending_plan(&state.http_client, tok, conv_id).await;
            if age <= 86400 {
                let intent = match IntentClassifier::compute_centroids_cached(
                    &state.intent_centroids, &embed_fn,
                ).await {
                    Some((cc, ccl, ci)) => IntentClassifier::new().classify_with_centroids(
                        &input, &embed_fn, &cc, &ccl, &ci,
                    ).await,
                    None => IntentClassifier::new().classify_with_embedding(&input, &embed_fn).await,
                };
                match intent {
                    Intent::Confirm => {
                        let is_note_open = pending.deferred_tools.first()
                            .map(|t| t.name == "__open_note__").unwrap_or(false);
                        if is_note_open {
                            let paths: Vec<serde_json::Value> = pending.deferred_tools.iter()
                                .flat_map(|t| t.args["paths"].as_array().cloned().unwrap_or_default())
                                .collect();
                            let note_name = paths.first()
                                .and_then(|p| p.as_str())
                                .and_then(|p| p.split('/').last())
                                .map(|n| n.trim_end_matches(".md").to_string())
                                .unwrap_or_else(|| "筆記".to_string());
                            let confirm_text = format!("好的，已為你打開《{}》。", note_name);
                            (emit_fn)("agent:open_note".into(), serde_json::Value::Array(paths));
                            (emit_fn)("llm:token".into(), serde_json::Value::String(confirm_text.clone()));
                            (emit_fn)("llm:done".into(), serde_json::Value::String(confirm_text.clone()));
                            return Ok(confirm_text);
                        }
                        // 寫入 deferred plan：將工具清單注入 context，繼續路由讓 sub-agent 執行
                        let deferred_desc = pending.deferred_tools.iter()
                            .map(|t| format!("[系統] 已確認，請立即執行：{} {:?}", t.name, t.args))
                            .collect::<Vec<_>>().join("\n");
                        messages_json.push(serde_json::json!({
                            "role": "user",
                            "content": deferred_desc
                        }));
                        // 繼續往下路由（full_context 會帶入訊息）
                    }
                    Intent::Cancel | Intent::Interrupt => {
                        (emit_fn)("agent:cancelled".into(), serde_json::Value::Null);
                        (emit_fn)("llm:done".into(), serde_json::Value::String(String::new()));
                        return Ok(String::new());
                    }
                    _ => {} // 無法辨識 → 繼續正常路由
                }
            }
        }
    }

    // 10. 記憶注入由 proactive skill 的 prefetch_memory tool chain 處理，此處無需額外查詢。

    // 12. 統一 LLM + tool loop（所有輪次相同路徑，tool call 歷史完整保存至 DB）
    //     背景異步：touch_agent 做 agent learning（不阻塞主流程）
    let session_id = uuid::Uuid::new_v4().to_string();
    *state.agent_session.lock().await = Some(session_id.clone());
    state.agent_cancel.store(false, std::sync::atomic::Ordering::SeqCst);


    let response_text = if vault_id_opt.is_some() && use_tools.unwrap_or(true) {
        // 保留前端傳來的 system（ORCHESTRATOR_SYSTEM，含明確工具使用規則）；
        // 若無 system，補上最低限度的 anti-hallucination 提示
        let anti_hallucination = "\n\n必須實際呼叫工具完成任務；禁止假裝或虛構結果。\
                                   若搜尋無結果，直接說明找不到。\
                                   回覆中引用筆記時，請包含完整的 vault 相對路徑。";
        let mut msgs: Vec<serde_json::Value> = if let Some(sys_msg) = messages_json.iter()
            .find(|m| m["role"].as_str() == Some("system"))
        {
            let patched_content = format!(
                "{}{}",
                sys_msg["content"].as_str().unwrap_or(""),
                anti_hallucination
            );
            std::iter::once(serde_json::json!({"role": "system", "content": patched_content}))
                .chain(messages_json.iter().filter(|m| m["role"].as_str() != Some("system")).cloned())
                .collect()
        } else {
            let fallback = format!("你是一個筆記助理，可以使用工具搜尋、讀取和管理筆記。{}", anti_hallucination);
            std::iter::once(serde_json::json!({"role": "system", "content": fallback}))
                .chain(messages_json.iter().cloned())
                .collect()
        };

        // Skill pre-pass：active skills（永遠注入）+ passive skills（embedding 相似度匹配）
        // 同時從命中的 skill.tool_calls 收集本輪所需工具子集（減少 context 用量）
        // 上限 1500 chars，保護 system message budget
        // 無 vault 或無 skill 觸發時的基本工具集（減少 context 用量）
        // 無 skill 觸發時的最小工具集：search_skills 負責找出所需工具，plan_announce 寫入確認必需
        let basic_tools = filter_vault_tools_by_names(&[
            "search_skills".to_string(),
            "plan_announce".to_string(),
        ]);

        let tools = if let Some(ref _vid) = vault_id_opt {
            if let Some((skill_text, skill_titles, required_tools, proactive_context)) = {
                // Skill pre-pass：透過 daemon API 搜尋匹配的 skills
                let vid = vault_id_opt.as_deref().unwrap_or("");
                let matched = search_skills_for_tool(
                    &client,
                    &auth_token,
                    vid,
                    &input,
                    skill_emb_url.as_deref(),
                    &client,
                ).await;
                if matched.is_empty() {
                    None
                } else {
                    // 收集 behavior 文字（注入 system prompt）
                    // Proactive skills: execute tool chain before LLM call
                    let proactive_context = {
                        let mut parts: Vec<String> = vec![];
                        for (_, _, _, _, need_chain, chain_order, mode) in &matched {
                            if mode != "proactive" || !need_chain { continue; }
                            for tool_name in chain_order {
                                if tool_name == "prefetch_memory" {
                                    let tok2 = if auth_token.is_empty() { None } else { Some(auth_token.as_str()) };
                                    // Use q= for semantic similarity search (embedder ranks by cosine);
                                    // service falls back to keyword match if embedder is unavailable.
                                    let url = format!(
                                        "/vaults/{}/memory/query?q={}&limit=8",
                                        urlencoding::encode(vid),
                                        urlencoding::encode(input.chars().take(120).collect::<String>().trim()),
                                    );
                                    if let Ok(results) = crate::api_client::daemon_get::<serde_json::Value>(&client, &url, tok2).await {
                                        let arr = results.as_array().cloned().unwrap_or_default();
                                        if !arr.is_empty() {
                                            let lines: Vec<String> = arr.iter().map(|r| {
                                                let cat = r["category"].as_str().unwrap_or("general");
                                                let content = r["content"].as_str().unwrap_or("");
                                                format!("[{}] {}", cat, content)
                                            }).collect();
                                            parts.push(format!("## 相關記憶\n{}", lines.join("\n")));
                                        }
                                    }
                                }
                            }
                        }
                        parts.join("\n\n")
                    };

                    let skill_text = matched.iter()
                        .filter(|(_, _, beh, _, _, _, mode)| !beh.is_empty() && mode != "proactive")
                        .map(|(_, title, beh, _, need_chain, chain_order, _)| {
                            if *need_chain && !chain_order.is_empty() {
                                format!("[技能：{}]\n{}\n工具執行順序：{}", title, beh, chain_order.join(" → "))
                            } else {
                                format!("[技能：{}]\n{}", title, beh)
                            }
                        })
                        .collect::<Vec<_>>().join("\n\n");
                    let skill_titles: Vec<String> = matched.iter()
                        .map(|(_, t, _, _, _, _, _)| t.clone())
                        .collect();
                    // 合併所有 skill 的 tool_calls，去重
                    let mut required_tools: Vec<String> = matched.iter()
                        .flat_map(|(_, _, _, tc, _, _, _)| tc.clone())
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();
                    required_tools.sort();
                    // 非同步 bump trigger_count（不阻塞主流程）
                    for (skill_id, _, _, _, _, _, _) in &matched {
                        let sid = skill_id.clone();
                        let hc = client.clone();
                        let at = auth_token.clone();
                        let v = vid.to_string();
                        tokio::spawn(async move {
                            let tok = if at.is_empty() { None } else { Some(at.as_str()) };
                            let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
                                &hc,
                                &format!("/vaults/{}/skills/{}/trigger", urlencoding::encode(&v), urlencoding::encode(&sid)),
                                &serde_json::json!({}),
                                tok,
                            ).await;
                        });
                    }
                    if skill_text.is_empty() && required_tools.is_empty() && proactive_context.is_empty() {
                        None
                    } else {
                        Some((skill_text, skill_titles, required_tools, proactive_context))
                    }
                }
            } {
                if let Some(sys) = msgs.first_mut() {
                    if sys["role"].as_str() == Some("system") {
                        let existing = sys["content"].as_str().unwrap_or("").to_string();
                        let mut injections = vec![existing];
                        if !proactive_context.is_empty() {
                            injections.push(proactive_context.chars().take(1000).collect());
                        }
                        if !skill_text.is_empty() {
                            injections.push(skill_text.chars().take(1500).collect());
                        }
                        *sys = serde_json::json!({"role": "system", "content": injections.join("\n\n")});
                    }
                }
                // 技能觸發提示（前端顯示 ⚡ 套用技能：...）
                if !skill_titles.is_empty() {
                    (emit_fn)("agent:skills_activated".into(), serde_json::json!({
                        "session_id": session_id,
                        "titles": skill_titles,
                    }));
                }
                // 命中的 skill 有指定 tool_calls → 只傳那些工具，大幅節省 context
                if !required_tools.is_empty() {
                    filter_vault_tools_by_names(&required_tools)
                } else {
                    basic_tools
                }
            } else {
                basic_tools
            }
        } else {
            serde_json::Value::Array(vec![])  // 無 vault → 不給工具
        };
        let mut tools = tools;  // 允許 tool loop 中動態擴展（search_skills 命中後注入 skill tools）

        // Context sliding window：保留 system，歷史訊息總 chars 上限 12000
        // （本地 LLM context ≈ 4096-8192 tokens；system+tools 佔 ~1500 tokens，
        //   剩餘 ~1000-2000 tokens 留給歷史，12000 chars ≈ 3000 tokens 足夠）
        {
            const MAX_HISTORY_CHARS: usize = 12000;
            let system_part: Vec<serde_json::Value> = msgs.iter()
                .filter(|m| m["role"].as_str() == Some("system"))
                .cloned().collect();
            let hist: Vec<serde_json::Value> = msgs.into_iter()
                .filter(|m| m["role"].as_str() != Some("system"))
                .collect();
            let total: usize = hist.iter()
                .map(|m| m["content"].as_str().unwrap_or("").len())
                .sum();
            let trimmed = if total > MAX_HISTORY_CHARS {
                // 從最舊訊息逐條捨棄，但至少保留最後 4 則
                let mut chars = total;
                let mut drop_n = 0usize;
                while chars > MAX_HISTORY_CHARS && drop_n + 4 < hist.len() {
                    chars = chars.saturating_sub(hist[drop_n]["content"].as_str().unwrap_or("").len());
                    drop_n += 1;
                }
                if drop_n > 0 {
                    eprintln!("[chat] context sliding window: dropped {} oldest messages (was {} chars)", drop_n, total);
                }
                hist[drop_n..].to_vec()
            } else {
                hist
            };
            msgs = system_part.into_iter().chain(trimmed.into_iter()).collect();
        }

        // ── Service delegation ────────────────────────────────────────────────
        // Extract tool names from the computed tools Value for the service call.
        let tool_names: Vec<String> = tools
            .as_array()
            .map(|arr| arr.iter()
                .filter_map(|t| t["function"]["name"].as_str().map(String::from))
                .collect())
            .unwrap_or_default();

        let vault_id_str = vault_id_opt.as_deref().unwrap_or("");
        crate::api_client::daemon_post::<_, serde_json::Value>(
            &state.http_client,
            &format!("/vaults/{}/agent/run", urlencoding::encode(vault_id_str)),
            &serde_json::json!({
                "session_id": session_id,
                "messages": msgs,
                "tool_names": tool_names,
                "vault_path": vault_path,
                "conversation_id": conversation_id,
            }),
            tok,
        ).await.map_err(|e| AppError::AI(e.to_string()))?;

        // Tokens arrive via SSE passthrough; return session_id immediately.
        return Ok(session_id);
    } else {
        // 無 vault 或 use_tools=false → 直接 LLM（純對話）
        // llm_fn 內部 send_streaming_request 已逐 token emit llm:token；此處只需 emit done
        let session_id2 = session_id.clone();
        let result = llm_fn(messages_json.clone(), None, Some(Arc::clone(&state.agent_cancel))).await
            .map_err(AppError::AI)?;
        let text = result.full_text;
        (emit_fn)("llm:done".into(), serde_json::Value::String(text.clone()));
        let _ = session_id2;
        text
    };

    *state.agent_session.lock().await = None;

    // 5-5: Bottom-up skill 歸納：若回覆包含明顯的步驟框架，發出 agent:skill_suggestion 事件
    // 讓前端決定是否引導使用者儲存為技能規範
    if vault_id_opt.is_some() && !response_text.is_empty() {
        let has_framework = detect_response_framework(&response_text);
        if has_framework {
            let _ = app.emit("agent:skill_suggestion", serde_json::json!({
                "query": &input,
                "response_preview": response_text.char_indices()
                    .nth(200).map(|(i, _)| &response_text[..i])
                    .unwrap_or(&response_text),
            }));
        }
    }

    // 若有 conversation_id，將完整對話（含 LLM 回覆）存回 DB
    if let Some(ref conv_id) = conversation_id {
        // 過濾掉 system prompt 再存（messages_json[0] 可能是 system）
        let mut to_save: Vec<serde_json::Value> = messages_json.into_iter()
            .filter(|m| m["role"].as_str() != Some("system"))
            .collect();

        // 只有實際有文字回覆才追加 assistant 訊息
        if !response_text.is_empty() {
            to_save.push(serde_json::json!({"role": "assistant", "content": response_text}));
        }

        let arr = serde_json::Value::Array(to_save);
        let _ = save_messages(&state.http_client, tok, conv_id, &arr).await;

        // maybe_set_title：只有首次（標題尚未設定）才需呼叫；之後用 in-memory set 跳過
        let already_titled = state.titled_convs.lock().await.contains(conv_id.as_str());
        if !already_titled {
            let _ = maybe_set_title(&state.http_client, tok, conv_id, &arr).await;
            state.titled_convs.lock().await.insert(conv_id.clone());
        }
    }

    Ok(response_text)
}

/// 語音對話 Agent 串流（Live Chat 專用）
///
/// 與 invoke_agent 的主要差異：
/// - 所有工具自動執行（無 write confirm 對話框）
/// - `live_respond` 為必要終止工具 → 執行完後 emit `live_chat:action` 並立即結束
/// - 使用固定 conversation_id（live_chat 模式），不走 skill pre-pass（速度優先）
/// - 注入當前編輯器筆記 context（note_context）
/// - 語言跟隨 whisper_language 設定
#[tauri::command]
pub async fn invoke_live_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    conversation_id: String,
    input: String,
    note_context: Option<String>,     // 當前開啟筆記的路徑+摘要，由前端提供
    activity_context: Option<String>, // 使用者活動紀錄（allOpenPaths + 最近操作），由前端提供
    language: Option<String>,         // whisper language 設定（"zh-TW", "en", etc.）
) -> Result<String, AppError> {
    use crate::commands::conversation::load_messages;

    // 1. 確保 llama-server 運行
    let _base_url = ensure_server_running(state.inner(), &app).await?;

    // 2. Vault 資訊
    let vault_path = state.get_vault_path().await;
    let vault_id_opt = state.get_vault_id().await.ok();
    let auth_token_lc = state.get_auth_token().await;
    let tok_lc = if auth_token_lc.is_empty() { None } else { Some(auth_token_lc.as_str()) };

    // 3. 從 DB 載入對話歷史
    let messages_json: Vec<serde_json::Value> = {
        let db_msgs = load_messages(&state.http_client, tok_lc, &conversation_id).await?;
        let mut arr = db_msgs.as_array().cloned().unwrap_or_default();
        arr.push(serde_json::json!({"role": "user", "content": input}));
        arr
    };

    // 4. System prompt（語言 + 口語 + 必須呼叫 live_respond）
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

    // Prefetch memory facts using semantic search against the current user input
    let memory_ctx_hint = if let Some(ref vid) = vault_id_opt {
        let q_param = urlencoding::encode(input.chars().take(120).collect::<String>().trim()).to_string();
        let url = format!("/vaults/{}/memory/query?q={}&limit=6", urlencoding::encode(vid), q_param);
        match crate::api_client::daemon_get::<serde_json::Value>(&state.http_client, &url, tok_lc).await {
            Ok(results) => {
                let arr = results.as_array().cloned().unwrap_or_default();
                if arr.is_empty() {
                    String::new()
                } else {
                    let node_ids: Vec<String> = arr.iter()
                        .filter_map(|r| r["fact_id"].as_str())
                        .map(|fid| format!("memory:{}:{}", vid, fid))
                        .collect();
                    if !node_ids.is_empty() {
                        let _ = app.emit("memory:prefetched", serde_json::json!({
                            "node_ids": node_ids,
                            "source": "chat"
                        }));
                    }
                    let lines: Vec<String> = arr.iter().filter_map(|r| {
                        let cat     = r["category"].as_str()?;
                        let content = r["content"].as_str()?;
                        Some(format!("[{}] {}", cat, content))
                    }).collect();
                    format!("\n\n[你對使用者的了解]\n{}", lines.join("\n"))
                }
            }
            Err(_) => String::new(),
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

    // 5. 取消旗標 + session
    state.agent_cancel.store(false, std::sync::atomic::Ordering::SeqCst);
    let session_id = uuid::Uuid::new_v4().to_string();
    *state.agent_session.lock().await = Some(session_id.clone());

    // 6. 組裝 messages（system + sliding window）
    let msgs: Vec<serde_json::Value> = {
        let sys_msg = serde_json::json!({"role": "system", "content": system});
        let hist: Vec<serde_json::Value> = messages_json.iter()
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
        std::iter::once(sys_msg).chain(trimmed.into_iter()).collect()
    };

    // 7. POST to service (blocking — service runs 3-round loop, SSE emitted inline)
    let vault_id_str = vault_id_opt.as_deref().unwrap_or("");
    let resp = crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/agent/live_chat", urlencoding::encode(vault_id_str)),
        &serde_json::json!({
            "session_id": session_id,
            "messages": msgs,
            "vault_path": vault_path,
            "conversation_id": conversation_id,
        }),
        tok_lc,
    ).await.map_err(|e| AppError::AI(e.to_string()))?;

    let final_speech = resp["speech"].as_str().unwrap_or("").to_string();
    Ok(final_speech)
}


// tool_list_recent_conversations — see runtime/tool_dispatch.rs
pub use crate::runtime::tool_dispatch::tool_list_recent_conversations;

// Tool schemas — see runtime/tool_schema.rs
pub use crate::runtime::tool_schema::vault_tools;

/// 從 vault_tools() 中過濾出指定名稱的工具子集。
/// plan_announce 永遠包含（寫入確認機制必需）。
fn filter_vault_tools_by_names(names: &[String]) -> serde_json::Value {
    crate::runtime::tool_schema::filter_vault_tools_by_names(names)
}

// Tool dispatch — see runtime/tool_dispatch.rs
pub(crate) use crate::runtime::tool_dispatch::{
    resolve_vault_path, tool_list_structure, tool_read_note,
    set_frontmatter_key, tool_create_note, tool_update_note, tool_create_folder,
    is_write_tool, execute_vault_tool,
};


// FTS helpers — see runtime/fts_helpers.rs
#[allow(unused_imports)]
pub(crate) use crate::runtime::fts_helpers::{
    clean_fts_query, Comparison, parse_comparison, filter_lines_by_comparison,
};

// set_note_status — see commands/vault.rs
pub use crate::commands::vault::set_note_status;

// detect_tool_calls — see runtime/llm_engine.rs
pub(crate) use crate::runtime::llm_engine::detect_tool_calls;

// Pipeline commands — see commands/pipeline.rs
pub use crate::commands::pipeline::{
    PipelineStep, PipelineStepResult, VaultChangedPayload,
    cancel_tool_test, run_tool_pipeline, test_vault_tool,
};
// Skill matching — see runtime/skill_matcher.rs
pub(crate) use crate::runtime::skill_matcher::{detect_response_framework, search_skills_for_tool};

// add_skill_trigger — see commands/agent_def.rs
pub use crate::commands::agent_def::add_skill_trigger;

