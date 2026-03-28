use crate::{error::AppError, state::AppState};
use crate::runtime::memory_agent::parse_text_tool_calls;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

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
    distill_preferences, extract_memory_facts, condense_memory_facts, suggest_skills_from_patterns,
    rate_response, get_conversation_ratings, analyze_tool_patterns,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}
/// 呼叫 LLM（非串流）根據 user_ask 生成 agent 規格 JSON。
/// 回傳 (name, description, trigger, tool_names)；任何錯誤 fallback 至 raw input。
#[allow(dead_code)]
async fn generate_agent_spec(
    client: &reqwest::Client,
    base_url: &str,
    input: &str,
) -> (String, String, String, Vec<String>, String, Vec<crate::runtime::system_agent::NewSkillSpec>) {
    let fallback = || (
        input.chars().take(24).collect::<String>(),
        input.to_string(),
        input.to_string(),
        vec![],
        String::new(),
        vec![],
    );

    let system = "\
你是一個 agent 規劃助理。根據使用者需求，輸出 JSON agent 規格（只輸出 JSON，不加任何說明）。\n\
格式：\n\
{\n\
  \"name\": \"<10字以內的中文名稱>\",\n\
  \"description\": \"<此agent專門做什麼>\",\n\
  \"trigger\": \"<何時觸發此agent的語意描述>\",\n\
  \"tool_names\": [<工具列表>],\n\
  \"system_prompt\": \"<此agent的繁體中文任務指令，說明如何解讀使用者語意、工具使用順序，2-4句話>\",\n\
  \"skills\": [\n\
    {\n\
      \"title\": \"<技能名稱>\",\n\
      \"trigger\": \"<何時套用此技能的語意描述>\",\n\
      \"behavior\": \"<具體行為規範，說明遇到此情境應如何處理>\",\n\
      \"injection_mode\": \"passive\"\n\
    }\n\
  ]\n\
}\n\
\n\
可用 tool_names：search_vault, read_note, open_note, list_structure, create_note, update_note, create_folder, query_memory, web_search, list_recent_conversations\n\
選擇原則：\n\
- 筆記查詢/搜尋 → [\"search_vault\"]\n\
- 筆記打開（讓使用者在編輯器中查看）→ [\"search_vault\",\"open_note\"]\n\
- 筆記閱讀/分析內容 → [\"search_vault\",\"read_note\"]\n\
- 筆記寫入/更新 → [\"create_note\",\"update_note\",\"create_folder\"]\n\
- 外部資訊/網路查詢 → [\"web_search\"]\n\
- 記憶查詢 → [\"query_memory\"]\n\
- 複合任務 → 組合上述\n\
\n\
skills 撰寫原則：\n\
- 每個 skill 對應一種使用者可能的語意變體或邊緣情境（例如：使用者說「找不到」時的 fallback 行為）\n\
- 0-3 個 skills，只有確實需要時才加（簡單任務可以 skills: []）\n\
- behavior 要具體可執行，不要空泛\n\
\n\
system_prompt 撰寫重點：說明使用者的用詞習慣、意圖語意、工具使用順序（例如：先 search_vault 再 open_note），禁止虛構結果。";

    let body = serde_json::json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": input},
        ],
        "max_tokens": 256,
        "temperature": 0.3,
        "stream": false,
    });

    let resp = match client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return fallback(),
    };

    let json: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return fallback(),
    };

    let text = json["choices"][0]["message"]["content"].as_str().unwrap_or("");
    let start = match text.find('{') { Some(i) => i, None => return fallback() };
    let end   = match text.rfind('}') { Some(i) => i + 1, None => return fallback() };

    let spec: serde_json::Value = match serde_json::from_str(&text[start..end]) {
        Ok(v) => v,
        Err(_) => return fallback(),
    };

    let name = spec["name"].as_str().unwrap_or("").chars().take(24).collect::<String>();
    let desc = spec["description"].as_str().unwrap_or(input).to_string();
    let trigger = spec["trigger"].as_str().unwrap_or(input).to_string();
    let tools: Vec<String> = spec["tool_names"].as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let system_prompt = spec["system_prompt"].as_str().unwrap_or("").to_string();
    let skills: Vec<crate::runtime::system_agent::NewSkillSpec> = spec["skills"].as_array()
        .map(|arr| arr.iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect())
        .unwrap_or_default();

    let name = if name.is_empty() { input.chars().take(24).collect() } else { name };
    (name, desc, trigger, tools, system_prompt, skills)
}

/// 計算多個 embedding 向量的 centroid（平均向量），並做 L2 正規化
pub fn compute_centroid(vecs: &[Vec<f32>]) -> Vec<f32> {
    if vecs.is_empty() {
        return vec![];
    }
    let dim = vecs[0].len();
    if dim == 0 {
        return vec![];
    }
    let mut centroid = vec![0f32; dim];
    for v in vecs {
        for (i, &f) in v.iter().enumerate() {
            if i < dim { centroid[i] += f; }
        }
    }
    let n = vecs.len() as f32;
    for f in &mut centroid { *f /= n; }
    // L2 normalize
    let norm: f32 = centroid.iter().map(|f| f * f).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for f in &mut centroid { *f /= norm; }
    }
    centroid
}

/// 封裝 OpenAI-compatible SSE 串流請求，返回 StreamResult
/// 同時處理文字 token（emit llm:token）和 tool call fragments 的累積
pub(crate) async fn send_streaming_request(
    client: &reqwest::Client,
    base_url: &str,
    body: serde_json::Value,
    app: &AppHandle,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<StreamResult, AppError> {
    let response = client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| AppError::AI(format!("請求 llama-server 失敗：{}", e)))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(AppError::AI(format!(
            "llama-server 回應錯誤 {}：{}",
            status, text
        )));
    }

    let mut stream = response.bytes_stream();
    let mut sse_buf = String::new();
    let mut full_text = String::new();
    let mut finish_reason = String::from("stop");
    let mut tool_call_chunks: Vec<ToolCallAccumulator> = Vec::new();

    while let Some(item) = stream.next().await {
        if cancel.as_ref().map_or(false, |c| c.load(Ordering::Relaxed)) {
            break;
        }
        let bytes = item.map_err(|e| AppError::AI(format!("串流讀取失敗：{}", e)))?;
        sse_buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(event_end) = sse_buf.find("\n\n") {
            let event = sse_buf[..event_end].to_string();
            sse_buf = sse_buf[event_end + 2..].to_string();

            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" {
                        continue;
                    }
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        let choice = &json["choices"][0];

                        // 記錄 finish_reason
                        if let Some(fr) = choice["finish_reason"].as_str() {
                            if !fr.is_empty() {
                                finish_reason = fr.to_string();
                            }
                        }

                        let delta = &choice["delta"];

                        // 一般文字 token
                        if let Some(content) = delta["content"].as_str() {
                            if !content.is_empty() {
                                let _ = app.emit("llm:token", content);
                                full_text.push_str(content);
                            }
                        }

                        // Tool call fragments 累積
                        if let Some(tc_arr) = delta["tool_calls"].as_array() {
                            for tc_chunk in tc_arr {
                                let idx =
                                    tc_chunk["index"].as_u64().unwrap_or(0) as usize;
                                while tool_call_chunks.len() <= idx {
                                    tool_call_chunks.push(ToolCallAccumulator {
                                        id: String::new(),
                                        name: String::new(),
                                        arguments: String::new(),
                                    });
                                }
                                let acc = &mut tool_call_chunks[idx];
                                if let Some(id) = tc_chunk["id"].as_str() {
                                    if !id.is_empty() {
                                        acc.id = id.to_string();
                                    }
                                }
                                if let Some(name) = tc_chunk["function"]["name"].as_str() {
                                    if !name.is_empty() {
                                        acc.name = name.to_string();
                                    }
                                }
                                if let Some(args_frag) =
                                    tc_chunk["function"]["arguments"].as_str()
                                {
                                    acc.arguments.push_str(args_frag);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(StreamResult {
        full_text,
        finish_reason,
        tool_call_chunks,
    })
}
/// 取消正在進行的 Agent 串流（設定取消旗標，同時拒絕待確認的寫入工具）
#[tauri::command]
pub async fn cancel_agent(state: State<'_, AppState>) -> Result<(), AppError> {
    state.agent_cancel.store(true, Ordering::Relaxed);
    if let Some(tx) = state.write_confirm_tx.lock().await.take() {
        let _ = tx.send(false);
    }
    Ok(())
}
/// 從查詢文字取出最多 N 個有意義的 CJK bigram，供 keyword 搜尋
#[allow(dead_code)]
fn extract_cjk_keywords(text: &str, max: usize) -> Vec<String> {
    const STOPS: &[char] = &[
        '你','我','他','她','它','的','了','嗎','是','有','在','說','道','記',
        '什','麼','這','那','就','都','也','還','不','沒','要','會','可','以',
        '和','與','或','但','如','果','因','為','所','而','且','呢','嗎','啊',
    ];
    let cjk: Vec<char> = text.chars()
        .filter(|c| *c as u32 >= 0x4E00 && *c as u32 <= 0x9FFF)
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for pair in cjk.windows(2) {
        if STOPS.contains(&pair[0]) && STOPS.contains(&pair[1]) { continue; }
        let bigram: String = pair.iter().collect();
        if seen.insert(bigram.clone()) {
            out.push(bigram);
            if out.len() >= max { break; }
        }
    }
    out
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
    use crate::runtime::tool_registry::ToolRegistry;
    use crate::runtime::types::{ConfirmWriteFn, EmitEventFn, LlmFn, LlmRound};

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

    // 4. 建立 ToolRegistry（vault 可用時注入工具）
    // 使用 llama-server 處理所有 agent trigger embedding（chat 就緒時必然可用）
    let reg_emb_url: Option<String> = Some(base_url.clone());
    let skill_emb_url = reg_emb_url.clone(); // 保留一份給 skill pre-pass 使用

    // 延遲繫結 handle（供 spawn_sub_agent 工具使用）
    let llm_fn_late = crate::tools::make_late_llm_fn();
    let registry_late: Arc<tokio::sync::Mutex<Option<Arc<ToolRegistry>>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    let registry = if !vault_path.is_empty() && vault_id_opt.is_some() {
        crate::tools::build_vault_registry(
            vault_path.clone(),
            vault_id_opt.clone().unwrap_or_default(),
            state.http_client.clone(),
            auth_token.clone(),
            app.clone(),
            reg_emb_url.clone(),
            Arc::clone(&llm_fn_late),
            Arc::clone(&registry_late),
            Arc::clone(&state.system_agent),
            Some(Arc::clone(&state.agent_cancel)),
        )
    } else {
        Arc::new(ToolRegistry::new())
    };
    // 設定延遲繫結的 registry（spawn_sub_agent 使用）
    *registry_late.lock().await = Some(Arc::clone(&registry));

    // 5. llm_fn：執行一輪 LLM 串流，返回 LlmRound
    //    tools_opt: None = 不傳工具；Some(json) = 傳指定工具列表（由 Agent 決定）
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
    // 設定延遲繫結的 llm_fn（spawn_sub_agent 使用）
    *llm_fn_late.lock().await = Some(llm_fn.clone());

    // 6. confirm_write_fn：設定 oneshot channel，等待 confirm_write_tool 命令
    let write_tx = Arc::clone(&state.write_confirm_tx);
    let confirm_write: ConfirmWriteFn = Arc::new(move |_display: String| {
        let tx = Arc::clone(&write_tx);
        Box::pin(async move {
            let (ch_tx, ch_rx) = tokio::sync::oneshot::channel::<bool>();
            *tx.lock().await = Some(ch_tx);
            tokio::time::timeout(Duration::from_secs(60), ch_rx)
                .await
                .unwrap_or(Ok(false))
                .unwrap_or(false)
        })
    });

    // 7. emit_fn：通用事件發送（包裝 AppHandle::emit）
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

    // 10. 記憶事實語意搜尋（daemon 架構下已移除本地 DB 依賴，回傳空字串）
    let (memory_context, _query_vec_opt) = if let Some(_vid) = vault_id_opt.as_ref() {
        let query_vec = get_embedding(&client, &base_url, &input).await;
        let vec_opt = if query_vec.is_empty() { None } else { Some(query_vec) };
        (String::new(), vec_opt)
    } else {
        (String::new(), None)
    };

    // 12. 統一 LLM + tool loop（所有輪次相同路徑，tool call 歷史完整保存至 DB）
    //     背景異步：touch_agent 做 agent learning（不阻塞主流程）
    let session_id = uuid::Uuid::new_v4().to_string();
    *state.agent_session.lock().await = Some(session_id.clone());
    state.agent_cancel.store(false, std::sync::atomic::Ordering::SeqCst);


    let response_text = if vault_id_opt.is_some() && use_tools.unwrap_or(true) {
        // messages_json 已包含當前 user input（line 654 append 過）
        use crate::runtime::dispatcher::Dispatcher;
        use crate::runtime::planner::Planner;
        use crate::runtime::transaction::Transaction;

        // 保留前端傳來的 system（ORCHESTRATOR_SYSTEM，含明確工具使用規則）；
        // 若無 system，補上最低限度的 anti-hallucination 提示
        let anti_hallucination = "\n\n必須實際呼叫工具完成任務；禁止假裝或虛構結果。\
                                   若搜尋無結果，直接說明找不到。\
                                   回覆中引用筆記時，請包含完整的 vault 相對路徑。";
        let mut msgs: Vec<serde_json::Value> = if let Some(sys_msg) = messages_json.iter()
            .find(|m| m["role"].as_str() == Some("system"))
        {
            // 前端已有 system → 在末尾追加 anti-hallucination 補丁
            let patched_content = format!(
                "{}{}",
                sys_msg["content"].as_str().unwrap_or(""),
                anti_hallucination
            );
            std::iter::once(serde_json::json!({"role": "system", "content": patched_content}))
                .chain(messages_json.iter().filter(|m| m["role"].as_str() != Some("system")).cloned())
                .collect()
        } else {
            // 無 system → 使用 fallback
            let fallback = format!("你是一個筆記助理，可以使用工具搜尋、讀取和管理筆記。{}", anti_hallucination);
            std::iter::once(serde_json::json!({"role": "system", "content": fallback}))
                .chain(messages_json.iter().cloned())
                .collect()
        };

        let dispatcher = Dispatcher::new(Arc::clone(&registry));
        let tx = Arc::new(Transaction::new());
        let _ = tx.prepare().await;
        let mut final_text = String::new();

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
                                    let url = format!(
                                        "/vaults/{}/memory/query?keywords={}&limit=8",
                                        urlencoding::encode(vid),
                                        urlencoding::encode(input.chars().take(60).collect::<String>().trim()),
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

        // 相關記憶注入（上限 1500 字元，切在筆記邊界避免截斷語義）
        if !memory_context.is_empty() {
            let snippet: String = if memory_context.chars().count() <= 1500 {
                memory_context.clone()
            } else {
                // 取前 1500 字元後，退回到最近的 \n\n 邊界（記憶條目分隔符）
                let truncated: String = memory_context.chars().take(1500).collect();
                match truncated.rfind("\n\n") {
                    Some(i) => format!(
                        "{}（…尚有更多記憶，可用 query_memory 工具取得完整內容）",
                        &truncated[..i]
                    ),
                    None => format!("{}…", truncated),
                }
            };
            let mem_block = format!("\n\n[相關歷史記憶]\n{}", snippet);
            if let Some(sys) = msgs.first_mut() {
                if sys["role"].as_str() == Some("system") {
                    let existing = sys["content"].as_str().unwrap_or("").to_string();
                    *sys = serde_json::json!({"role": "system", "content": format!("{}{}", existing, mem_block)});
                }
            }
        }

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

        'tool_loop: for _round in 0..8usize {
            if state.agent_cancel.load(std::sync::atomic::Ordering::Relaxed) { break; }

            let result = match llm_fn(
                msgs.clone(),
                Some(tools.clone()),
                Some(Arc::clone(&state.agent_cancel)),
            ).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[chat] llm error: {e}");
                    return Err(AppError::AI(format!("{}", e)));
                }
            };
            final_text = result.full_text.clone();
            if result.tool_calls.is_empty() { break; }

            for (_, name, _) in &result.tool_calls {
                (emit_fn)("agent:tool_call".into(), serde_json::json!({
                    "session_id": session_id,
                    "display": format!("🔧 {name}"),
                }));
            }

            // 寫入工具確認
            let has_write = result.tool_calls.iter().any(|(_, n, _)|
                matches!(n.as_str(), "create_note" | "update_note" | "create_folder" | "delete_note" | "delete_folder" | "move_note" | "append_to_note"));
            if has_write {
                let display = result.tool_calls.iter()
                    .filter(|(_, n, _)| matches!(n.as_str(), "create_note"|"update_note"|"create_folder"|"delete_note"|"delete_folder"|"move_note"|"append_to_note"))
                    .map(|(_, n, a)| format!("- {} {}", n, a["path"].as_str().or_else(|| a["from"].as_str()).unwrap_or("")))
                    .collect::<Vec<_>>().join("\n");
                (emit_fn)("agent:write_request".into(), serde_json::Value::String(display.clone()));
                let approved = confirm_write.clone()(display).await;
                if !approved {
                    let tc_json: Vec<serde_json::Value> = result.tool_calls.iter().map(|(id, n, a)| {
                        serde_json::json!({"id": id, "type": "function", "function": {"name": n, "arguments": a.to_string()}})
                    }).collect();
                    msgs.push(serde_json::json!({"role": "assistant", "content": null, "tool_calls": tc_json}));
                    for (tool_id, name, _) in &result.tool_calls {
                        msgs.push(serde_json::json!({"role": "tool", "tool_call_id": tool_id, "name": name, "content": "用戶拒絕了此寫入操作。"}));
                    }
                    continue 'tool_loop;
                }
            }

            let tool_graph = Planner::plan(&result.tool_calls);
            let results = match dispatcher.run(Arc::clone(&tx), tool_graph).await {
                Ok(r) => r,
                Err(e) => { eprintln!("[chat] tool error: {e}"); break; }
            };

            let tc_json: Vec<serde_json::Value> = result.tool_calls.iter().map(|(id, n, a)| {
                serde_json::json!({"id": id, "type": "function", "function": {"name": n, "arguments": a.to_string()}})
            }).collect();
            msgs.push(serde_json::json!({"role": "assistant", "content": null, "tool_calls": tc_json}));

            for ((tool_id, name, args), res) in result.tool_calls.iter().zip(results.iter()) {
                let raw = res.as_str().map(String::from).unwrap_or_else(|| res.to_string());

                // search_skills 動態注入：解析回傳 JSON，擴展本輪 tools 供後續 LLM 呼叫使用
                let raw = if name == "search_skills" {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                        // 擴展 tools（合併現有 + skill 指定工具）
                        if let Some(arr) = v["required_tools"].as_array() {
                            let names: Vec<String> = arr.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect();
                            if !names.is_empty() {
                                // 合併現有 tools 中已有的（如 search_skills / plan_announce）+ skill tools
                                let existing_names: Vec<String> = tools.as_array()
                                    .map(|a| a.iter()
                                        .filter_map(|t| t["function"]["name"].as_str().map(String::from))
                                        .collect())
                                    .unwrap_or_default();
                                let mut all_names = existing_names;
                                for n in &names { if !all_names.contains(n) { all_names.push(n.clone()); } }
                                tools = filter_vault_tools_by_names(&all_names);
                            }
                        }
                        // LLM 看到 behavior 文字，不是 JSON
                        v["behavior"].as_str().map(String::from).unwrap_or(raw)
                    } else {
                        raw
                    }
                } else {
                    raw
                };

                // Tool result 上限 3000 chars，防止大型 read_note 繞過 sliding window
                const MAX_TOOL_RESULT: usize = 3000;
                let res_str = if raw.chars().count() > MAX_TOOL_RESULT {
                    // read_note：嘗試從 chunks 表並行摘要；其他工具截斷
                    if name == "read_note" {
                        if let (Some(_vid), Some(_fp)) = (vault_id_opt.as_deref(), args["path"].as_str()) {
                            // parallel_chunk_summarize 已移除本地 DB 依賴，直接截斷（daemon 架構）
                            {
                                let truncated: String = raw.chars().take(MAX_TOOL_RESULT).collect();
                                format!("{}…（內容已截斷）", truncated)
                            }
                        } else {
                            let truncated: String = raw.chars().take(MAX_TOOL_RESULT).collect();
                            format!("{}…（內容已截斷）", truncated)
                        }
                    } else {
                        let truncated: String = raw.chars().take(MAX_TOOL_RESULT).collect();
                        format!("{}…（內容已截斷，如需完整請分段呼叫）", truncated)
                    }
                } else {
                    raw
                };
                msgs.push(serde_json::json!({"role": "tool", "tool_call_id": tool_id, "name": name, "content": res_str}));
            }

            // open_note 是 terminal tool：執行完直接結束，不需再呼叫 LLM
            if result.tool_calls.iter().any(|(_, n, _)| n == "open_note") {
                let opened: Vec<&str> = result.tool_calls.iter()
                    .filter(|(_, n, _)| n == "open_note")
                    .map(|(_, _, a)| a["path"].as_str().unwrap_or("筆記"))
                    .collect();
                final_text = format!("已為你打開：{}", opened.join("、"));
                break;
            }
        }

        let _ = tx.commit().await;
        (emit_fn)("llm:done".into(), serde_json::Value::String(final_text.clone()));

        // 更新 messages_json 供 save 段落使用：保留完整 tool call history（含 tool role）
        // 前端顯示時透過 get_conversation 的 display_messages_json 過濾，不在此處截斷
        messages_json = msgs.into_iter()
            .filter(|m| m["role"].as_str() != Some("system"))
            .collect();

        final_text
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
    use crate::commands::conversation::{load_messages, save_messages};
    use crate::runtime::types::{EmitEventFn, LlmFn, LlmRound};
    use crate::runtime::tool_registry::ToolRegistry;

    // 1. 確保 llama-server 運行
    let base_url = ensure_server_running(state.inner(), &app).await?;
    let client = state.http_client.clone();

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

    // Prefetch memory facts and inject into system prompt
    let memory_ctx_hint = if let Some(ref vid) = vault_id_opt {
        let url = format!("/vaults/{}/memory/query?limit=6", urlencoding::encode(vid));
        match crate::api_client::daemon_get::<serde_json::Value>(&state.http_client, &url, tok_lc).await {
            Ok(results) => {
                let arr = results.as_array().cloned().unwrap_or_default();
                if arr.is_empty() {
                    String::new()
                } else {
                    // Emit prefetched node_ids for MemoryLinksView
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

    // 5. 建立 ToolRegistry
    let llm_fn_late = crate::tools::make_late_llm_fn();
    let registry_late: Arc<tokio::sync::Mutex<Option<Arc<ToolRegistry>>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    let registry = if !vault_path.is_empty() && vault_id_opt.is_some() {
        crate::tools::build_vault_registry(
            vault_path.clone(),
            vault_id_opt.clone().unwrap_or_default(),
            state.http_client.clone(),
            auth_token_lc.clone(),
            app.clone(),
            Some(base_url.clone()),
            Arc::clone(&llm_fn_late),
            Arc::clone(&registry_late),
            Arc::clone(&state.system_agent),
            Some(Arc::clone(&state.agent_cancel)),
        )
    } else {
        Arc::new(ToolRegistry::new())
    };
    *registry_late.lock().await = Some(Arc::clone(&registry));

    // 6. llm_fn
    let client_fn = client.clone();
    let base_fn = base_url.clone();
    let app_fn = app.clone();
    let cancel_flag = Arc::clone(&state.agent_cancel);
    let llm_fn: LlmFn = Arc::new(move |msgs, tools_opt, cancel| {
        let client = client_fn.clone();
        let base = base_fn.clone();
        let app = app_fn.clone();
        Box::pin(async move {
            // 語音助理回覆短促：max_tokens 512 加快首 token 速度
            // tool_choice: "required" 強制 LLM 必須呼叫工具，不允許生成純文字
            let body = if let Some(tools) = tools_opt {
                serde_json::json!({
                    "messages": msgs,
                    "tools": tools,
                    "tool_choice": "required",
                    "max_tokens": 512,
                    "temperature": 0.7,
                    "stream": true,
                })
            } else {
                serde_json::json!({
                    "messages": msgs,
                    "max_tokens": 512,
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
    *llm_fn_late.lock().await = Some(llm_fn.clone());

    // 7. emit_fn
    let app_emit = app.clone();
    let emit_fn: EmitEventFn = Arc::new(move |event: String, payload: serde_json::Value| {
        let _ = app_emit.emit(&event, payload);
    });

    // 8. 取消旗標
    state.agent_cancel.store(false, std::sync::atomic::Ordering::SeqCst);

    // 8b. Live chat 執行期間自動核准寫入工具（無確認 UI）。
    // make_confirm_write 會讀取此旗標，為 true 時直接回傳 true 不等前端。
    // 使用 AtomicBool 而非 polling task，不影響並行的 stream_chat 確認流程。
    state.live_chat_active.store(true, std::sync::atomic::Ordering::SeqCst);

    // 9. 組裝 messages（system + sliding window）
    let mut msgs: Vec<serde_json::Value> = {
        let sys_msg = serde_json::json!({"role": "system", "content": system});
        let hist: Vec<serde_json::Value> = messages_json.iter()
            .filter(|m| m["role"].as_str() != Some("system"))
            .cloned().collect();
        // Sliding window 8000 chars
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

    // 10. 工具清單：第一輪只有 search_skills，強制 LLM 先路由再執行再 live_respond
    // live_respond 在 search_skills 執行後才注入，防止 LLM 在第一輪就呼叫它提早結束
    let live_chat_tool_names: Vec<String> = vec!["search_skills"]
        .into_iter().map(String::from).collect();
    let _tools = filter_vault_tools_by_names(&live_chat_tool_names);

    // 11. Tool loop（最多 2 輪：round 0 = 第一輪 LLM → 可呼叫搜尋；round 1 = 再次 LLM → 必須呼叫 live_respond）
    use crate::runtime::dispatcher::Dispatcher;
    use crate::runtime::planner::Planner;
    use crate::runtime::transaction::Transaction;

    let dispatcher = Dispatcher::new(Arc::clone(&registry));
    let tx = Arc::new(Transaction::new());
    let _ = tx.prepare().await;
    let mut final_speech = String::new();
    let mut live_action: Option<serde_json::Value> = None;

    // 輔助：emit 錯誤 action（TTS + 顯示錯誤卡片）
    let emit_error_action = |speech: &str, detail: String| {
        (emit_fn)("live_chat:action".into(), serde_json::json!({
            "speech": speech,
            "action": "show_error",
            "error": detail,
        }));
    };

    // 三輪固定流程，每輪工具清單不同，無條件判斷
    // Round 0：只有 search_skills → 取得 skill tool names
    // Round 1：skill tools + plan_announce → LLM 宣告計畫並執行工具
    // Round 2：只有 live_respond → LLM 必須輸出最終答案
    let mut skill_tool_names: Vec<String> = Vec::new();

    'tool_loop: for round in 0..3usize {
        if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) { break; }

        // 每輪的工具清單由 round 決定，不做動態條件判斷
        // Round 1：若 search_skills 未回傳任何工具，直接跳過（避免空 tools + required 出錯）
        if round == 1 && skill_tool_names.is_empty() {
            continue;
        }

        let round_tools = match round {
            0 => filter_vault_tools_by_names(&["think".to_string(), "search_skills".to_string()]),
            1 => {
                // skill tools + think（LLM 執行工具前先說內心獨白）
                let mut names = vec!["think".to_string()];
                names.extend(skill_tool_names.iter().cloned());
                filter_vault_tools_by_names(&names)
            }
            _ => {
                // 只有 live_respond，LLM 必須輸出答案
                filter_vault_tools_by_names(&["live_respond".to_string()])
            }
        };

        let result = match llm_fn(
            msgs.clone(),
            Some(round_tools),
            Some(Arc::clone(&cancel_flag)),
        ).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[live_chat] llm error: {e}");
                emit_error_action("抱歉，語言模型回應失敗，請稍後再試。", format!("LLM 錯誤：{e}"));
                state.live_chat_active.store(false, std::sync::atomic::Ordering::SeqCst);
                return Ok(String::new());
            }
        };

        // Round 2：LLM 只有 live_respond，接受其輸出（tool call 或純文字）
        if round == 2 {
            if let Some((_, _, args)) = result.tool_calls.iter().find(|(_, n, _)| n == "live_respond") {
                final_speech = args["speech"].as_str().unwrap_or("").to_string();
                live_action = Some(args.clone());
            } else {
                // 模型沒有呼叫 live_respond，直接用文字
                final_speech = result.full_text.clone();
            }
            break 'tool_loop;
        }

        // Round 0 / 1：執行工具
        // plan_announce 不在 registry，特殊處理；其餘走 dispatcher
        let (pa_calls, real_calls): (Vec<_>, Vec<_>) = result.tool_calls.iter()
            .partition(|(_, n, _)| n == "plan_announce");

        for (_, name, _) in &result.tool_calls {
            if name != "plan_announce" {
                (emit_fn)("live_chat:tool_call".into(), serde_json::json!({"display": format!("🔧 {name}")}));
            }
        }

        let tc_json: Vec<serde_json::Value> = result.tool_calls.iter().map(|(id, n, a)|
            serde_json::json!({"id": id, "type": "function", "function": {"name": n, "arguments": a.to_string()}})
        ).collect();
        // 沒有 tool call 也沒有文字 → 直接進下一輪
        if tc_json.is_empty() { continue; }
        msgs.push(serde_json::json!({"role": "assistant", "content": null, "tool_calls": tc_json}));

        // plan_announce → 自動確認
        for (tool_id, _, _) in &pa_calls {
            msgs.push(serde_json::json!({"role": "tool", "tool_call_id": tool_id,
                "name": "plan_announce", "content": "✅ 已自動確認，繼續執行"}));
        }

        if real_calls.is_empty() { continue; }

        let real_calls_owned: Vec<_> = real_calls.into_iter().cloned().collect();
        let tool_graph = Planner::plan(&real_calls_owned);
        let results = match dispatcher.run(Arc::clone(&tx), tool_graph).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[live_chat] tool error: {e}");
                emit_error_action("抱歉，執行工具時遇到問題，已為您顯示錯誤訊息。", format!("工具執行失敗：{e}"));
                break;
            }
        };

        for ((tool_id, name, _), res) in real_calls_owned.iter().zip(results.iter()) {
            let raw = res.as_str().map(String::from).unwrap_or_else(|| res.to_string());

            // Round 0：search_skills → 記錄 skill tool names（Round 1 使用）
            let raw = if name == "search_skills" {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if let Some(arr) = v["required_tools"].as_array() {
                        skill_tool_names = arr.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect();
                    }
                    v["behavior"].as_str().map(String::from).unwrap_or(raw)
                } else { raw }
            } else { raw };

            let res_str = if raw.chars().count() > 2000 {
                format!("{}…（已截斷）", raw.chars().take(2000).collect::<String>())
            } else { raw };
            msgs.push(serde_json::json!({"role": "tool", "tool_call_id": tool_id, "name": name, "content": res_str}));
        }
    }

    let _ = tx.commit().await;

    // 12. 儲存回 DB（過濾 system，追加 assistant 回覆）
    {
        let mut to_save: Vec<serde_json::Value> = messages_json.iter()
            .filter(|m| m["role"].as_str() != Some("system"))
            .cloned().collect();
        if !final_speech.is_empty() {
            to_save.push(serde_json::json!({"role": "assistant", "content": final_speech}));
        }
        let arr = serde_json::Value::Array(to_save.clone());
        let _ = save_messages(&state.http_client, tok_lc, &conversation_id, &arr).await;

        // 每 10 則 user 訊息自動存至 vault memory（inline，不透過 Tauri command）
        let user_count = to_save.iter().filter(|m| m["role"].as_str() == Some("user")).count();
        if user_count > 0 && user_count % 10 == 0 && !vault_path.is_empty() {
            use chrono::Local;
            let now = Local::now();
            let timestamp = now.format("%Y%m%d_%H%M%S").to_string();
            let display_time = now.format("%Y-%m-%d %H:%M:%S").to_string();
            let rel_path = format!("memories/ai_memory_live_{}.md", timestamp);
            let abs_path = std::path::PathBuf::from(&vault_path).join(&rel_path);
            let mut content = format!(
                "---\ncreated: {}\nmessage_count: {}\nsource: live_chat\n---\n\n# AI 語音對話記憶 — {}\n\n",
                now.to_rfc3339(),
                to_save.iter().filter(|m| m["role"].as_str() != Some("tool")).count(),
                display_time
            );
            for msg in &to_save {
                match msg["role"].as_str() {
                    Some("user") => content.push_str(&format!("**使用者**\n\n{}\n\n---\n\n", msg["content"].as_str().unwrap_or(""))),
                    Some("assistant") => content.push_str(&format!("**助手**\n\n{}\n\n---\n\n", msg["content"].as_str().unwrap_or(""))),
                    _ => {}
                }
            }
            if let Some(parent) = abs_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let _ = tokio::fs::write(&abs_path, &content).await;
            // Sync memory note to daemon (no file watcher in daemon)
            let vault_id = state.get_vault_uuid().await;
            if !vault_id.is_empty() {
                let token2 = state.get_auth_token().await;
                let tok2: Option<&str> = if token2.is_empty() { None } else { Some(token2.as_str()) };
                let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
                    &state.http_client,
                    &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
                    &serde_json::json!({"path": rel_path, "content": content}),
                    tok2,
                ).await;
            }
        }
    }

    // 復原旗標，恢復 stream_chat 的正常確認流程
    state.live_chat_active.store(false, std::sync::atomic::Ordering::SeqCst);

    // 13. emit live_chat:action（前端根據 action 執行 UI 操作）
    // 若迴圈未產生任何回應（工具錯誤 break 或取消），且沒有錯誤 action 已發出，補發 fallback
    if let Some(action_args) = live_action {
        (emit_fn)("live_chat:action".into(), action_args);
    } else if final_speech.is_empty() && !cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
        emit_error_action(
            "抱歉，我沒有得到有效的回覆，請再試一次。",
            "未能取得有效回應".to_string(),
        );
    }

    Ok(final_speech)
}

/// 串流聊天（外部 AI 提供商）：OpenAI / Anthropic / Ollama
/// tokens 以 "llm:token" 事件推送，完成後發送 "llm:done"
/// 一次性 LLM 處理（語音後處理）：非串流，等待完整回應後回傳
/// system 放角色指令，user_content 放待處理文字，分開傳可讓模型正確執行任務而非對話
/// 手動停止 llama-server（App 退出時也會自動呼叫）
/// 查詢 llama-server 狀態："running" | "loading" | "stopped"
/// 手動啟動 llama-server
/// 重啟 llama-server（先強制關閉再重新啟動）
// ─── Embedding Server ─────────────────────────────────────────────────────────
/// 診斷用：測試 embedding server 的端點，回傳狀態碼與回應摘要
// ─── Vault Agent ──────────────────────────────────────────────────────────────


/// 串流過程中累積的單一 tool call 資料
pub(crate) struct ToolCallAccumulator {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String, // 累積的 JSON fragment 字串
}

/// send_streaming_request 的回傳結果
pub(crate) struct StreamResult {
    pub(crate) full_text: String,
    pub(crate) finish_reason: String,
    pub(crate) tool_call_chunks: Vec<ToolCallAccumulator>,
}

/// 讀取最近對話，回傳摘要供 reflection agent 分析模式
pub async fn tool_list_recent_conversations(
    http_client: &reqwest::Client,
    auth_token: &str,
    limit: usize,
) -> String {
    let limit = limit.min(20);
    let tok = if auth_token.is_empty() { None } else { Some(auth_token) };
    let result: serde_json::Value = crate::api_client::daemon_get(
        http_client,
        &format!("/conversations?limit={}", limit),
        tok,
    ).await.unwrap_or_else(|_| serde_json::json!([]));
    let rows = match result.as_array() {
        Some(r) => r.clone(),
        None => return "沒有找到任何對話記錄".to_string(),
    };
    if rows.is_empty() { return "沒有找到任何對話記錄".to_string(); }
    let mut out = format!("最近 {} 段對話：\n\n", rows.len());
    for (i, row) in rows.iter().enumerate() {
        let title = row["title"].as_str().unwrap_or("未命名");
        let mode  = row["mode"].as_str().unwrap_or("chat");
        out.push_str(&format!("## 對話 {} — {} ({})\n", i + 1, title, mode));
        if let Some(ref json) = row["messages_json"].as_str() {
            if let Ok(msgs) = serde_json::from_str::<Vec<serde_json::Value>>(json) {
                let tail: Vec<_> = msgs.iter()
                    .filter(|m| {
                        let role = m["role"].as_str().unwrap_or("");
                        role == "user" || role == "assistant"
                    })
                    .rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
                for m in tail {
                    let role    = m["role"].as_str().unwrap_or("?");
                    let content = m["content"].as_str().unwrap_or("").chars().take(200).collect::<String>();
                    out.push_str(&format!("**{}**: {}\n", role, content));
                }
            }
        }
        out.push('\n');
    }
    out
}

/// 工具定義（OpenAI function calling 格式）
pub fn vault_tools() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "search_vault",
                "description": "全文搜索 Vault 中的筆記，返回相關筆記列表及摘要。\
【前置工具】：open_note / read_note / update_note / append_to_note / delete_note / move_note 都需要精確路徑，若路徑不確定，必須先呼叫 search_vault 取得。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "搜索關鍵字" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_structure",
                "description": "列出指定資料夾路徑下的子資料夾和筆記（.md）。path 傳空字串表示 Vault 根目錄。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相對於 Vault 根目錄的資料夾路徑（空字串 = 根目錄）" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_note",
                "description": "讀取指定筆記的完整 Markdown 內容，用於需要分析、摘要或修改筆記內容時。\
【前置要求】必須知道精確路徑；不確定時先用 search_vault。\
注意：若使用者只是要「打開」或「查看」筆記，請改用 open_note 工具；read_note 僅用於需要理解或修改內容的情況。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "筆記相對路徑（含 .md 副檔名，例如 工作/專案A.md）" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "create_note",
                "description": "在 Vault 中建立新筆記，會自動建立所需的父資料夾",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "新筆記的相對路徑（含 .md 副檔名）" },
                        "content": { "type": "string", "description": "筆記的 Markdown 內容" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "update_note",
                "description": "覆寫更新現有筆記的完整內容。\
【操作序列】：(1) 若路徑不確定 → 先 search_vault；(2) 若需保留現有內容做部分修改 → 先 read_note 取得原始內容，再修改後呼叫 update_note。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "筆記相對路徑" },
                        "content": { "type": "string", "description": "新的完整 Markdown 內容" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "create_folder",
                "description": "在 Vault 中建立新資料夾（含所有中間層資料夾）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "新資料夾的相對路徑" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "在網路上搜尋最新資訊。當本地知識庫缺乏相關內容、或需要最新資訊時使用。\
搜尋結果會自動在背景加入「匯入知識」，使用者稍後可在匯入中心查看完整來源。\
不要用來查詢 Vault 筆記（請用 search_vault）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "搜尋關鍵字或問題（建議使用具體關鍵字）"
                        }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "plan_announce",
                "description": "當你打算執行寫入操作（create_note / update_note / create_folder）且需要使用者確認時，\
先呼叫此工具記錄計畫。提供使用者可能用來確認/取消/中斷的樣本短語（用於語意匹配），\
以及你打算執行的工具清單（deferred_tools）。呼叫後再用文字告知使用者計畫內容，等待確認。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "confirm_phrases": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "使用者確認計畫時可能說的 10-15 個短語（口語、正式、縮短形式都要）"
                        },
                        "cancel_phrases": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "使用者取消計畫時可能說的 10-15 個短語"
                        },
                        "interrupt_phrases": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "使用者暫停/插話時可能說的短語"
                        },
                        "deferred_tools": {
                            "type": "array",
                            "description": "計畫執行的工具清單",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": {"type": "string"},
                                    "args": {"type": "object"}
                                },
                                "required": ["name", "args"]
                            }
                        }
                    },
                    "required": ["confirm_phrases", "cancel_phrases", "interrupt_phrases", "deferred_tools"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "open_note",
                "description": "在筆記編輯器中打開（切換至）指定筆記，讓使用者在編輯器中直接看到內容。\
使用者說「打開」「開啟」「跳轉到」「要查看」「幫我看」「看一下」某筆記時，優先使用此工具，不要用 read_note。\
若不確定路徑，先用 search_vault 找到路徑再呼叫。呼叫後只需回覆「已打開 xxx 筆記」，不要輸出任何筆記內容。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "筆記的相對路徑，例如 'folder/note.md'"
                        }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_recent_conversations",
                "description": "讀取最近的對話記錄，分析使用者的重複需求、知識缺口和行為模式。僅供自我改進分析使用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "number",
                            "description": "要讀取的對話數量（預設 10，最多 20）"
                        }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "create_agent_skill",
                "description": "根據觀察到的使用者模式，建立新的技能規範。建立後預設未啟用，使用者可在「我的技能規範」頁面審核並啟用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "技能名稱" },
                        "trigger": { "type": "string", "description": "觸發條件，以「當...時」開頭" },
                        "behavior": { "type": "string", "description": "具體操作規範：先做A，再做B" },
                        "injection_mode": {
                            "type": "string",
                            "enum": ["passive", "active"],
                            "description": "passive=語意相似時注入；active=永遠注入"
                        },
                        "need_tool_chain": {
                            "type": "boolean",
                            "description": "工具是否需要嚴格依序執行（有前置條件時設為 true）"
                        },
                        "tool_chain_order": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "工具執行順序（need_tool_chain=true 時填入），例如 [\"search_vault\", \"read_note\", \"update_note\"]"
                        }
                    },
                    "required": ["title", "trigger", "behavior"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "touch_agent",
                "description": "當使用者需要以下任何一種任務時，必須呼叫此工具：\
網路搜尋、即時資訊（天氣/新聞/股價）、外部 API、複雜計算、程式碼生成、\
資料分析、建立或修改筆記、整理或摘要多篇筆記。\
系統會自動以 task 語意搜尋現有 agent；找到則複用，找不到則自動建立後執行。\
只有「純粹閒聊」或「解釋概念」才可不呼叫此工具直接回答。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "完整任務描述，包含所有必要資訊（用於語意匹配與執行）"
                        },
                        "name": {
                            "type": "string",
                            "description": "（可選）agent 名稱提示，建立新 agent 時使用"
                        },
                        "description": {
                            "type": "string",
                            "description": "（可選）agent 職責描述，建立新 agent 時使用"
                        },
                        "trigger": {
                            "type": "string",
                            "description": "（可選）pre-routing 觸發關鍵詞，建立新 agent 時使用"
                        },
                        "tool_names": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "（可選）建立新 agent 時建議使用的工具"
                        },
                        "context": {
                            "type": "string",
                            "description": "（可選）提供給 agent 的背景資訊"
                        }
                    },
                    "required": ["task"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "call_agent",
                "description": "（內部使用）透過 System Agent Service 路由任務給指定 agent。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "target": { "type": "string" },
                        "task": { "type": "string" },
                        "context": { "type": "string" }
                    },
                    "required": ["target", "task"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "create_agent",
                "description": "（內部使用）建立 agent definition。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "trigger": { "type": "string" },
                        "tool_names": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["name", "description", "trigger", "tool_names"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_available_agents",
                "description": "列出目前所有可用的 agent definitions（包含自訂）。",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "query_memory",
                "description": "搜尋過去對話記憶。keywords 空陣列=取最新記憶；有關鍵字=FTS 搜尋。since 為時間下限 YYYY-MM-DD。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "keywords": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "關鍵字列表（空陣列 = 取最新記憶）"
                        },
                        "since": {
                            "type": "string",
                            "description": "時間下限，YYYY-MM-DD 格式（可選）"
                        },
                        "limit": {
                            "type": "number",
                            "description": "最多返回幾條記憶（預設 5）"
                        }
                    },
                    "required": ["keywords"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "prefetch_memory",
                "description": "根據當前對話主題，自動擷取最相關的記憶事實並注入為背景知識。通常由系統自動呼叫，不需手動使用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "context": {
                            "type": "string",
                            "description": "當前對話的關鍵詞或主題描述（可選，留空則取最新記憶）"
                        }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "think",
                "description": "在執行下一個工具前，輸出一句內心獨白描述你正在思考的方向。必須在每個工具呼叫之前先呼叫此工具。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "thought": {
                            "type": "string",
                            "description": "內心獨白，口語化繁體中文，10字以內，描述你接下來要做什麼或想到什麼"
                        }
                    },
                    "required": ["thought"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "find_related",
                "description": "透過知識圖譜找出與指定筆記相關聯的筆記（wiki link 連結）。\
適用情境：探索某個主題的延伸閱讀、找出相互引用的筆記群。\
【操作序列】：先 list_structure 確認路徑 → 呼叫 find_related 取得相關節點 → 視需要 read_note 閱讀內容。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "起點筆記的相對路徑（含 .md 副檔名）"
                        },
                        "depth": {
                            "type": "integer",
                            "description": "圖譜遍歷深度（預設 1，最大 2）"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "最多回傳幾個相關筆記（預設 10）"
                        }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_current_datetime",
                "description": "取得目前本地時間（年月日時分秒時區）",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_notes_in_folder",
                "description": "列出指定資料夾下的所有筆記。\
【操作序列】：若資料夾路徑不確定 → 先 list_structure 確認資料夾名稱，再呼叫本工具。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "folder": {"type": "string", "description": "資料夾相對路徑（如 'projects' 或 'projects/web'）"}
                    },
                    "required": ["folder"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "append_to_note",
                "description": "在現有筆記末尾追加內容（不覆蓋原有內容）。\
【操作序列】：若路徑不確定 → 先 search_vault 取得路徑，再呼叫 append_to_note。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "筆記相對路徑"},
                        "content": {"type": "string", "description": "要追加的內容"}
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "delete_note",
                "description": "刪除指定筆記（永久，不可復原）。\
【操作序列】：操作不可逆，若路徑不確定，必須先 search_vault 確認路徑後再呼叫。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "筆記相對路徑或名稱"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "delete_folder",
                "description": "刪除指定資料夾及其所有內容（需使用者確認）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "資料夾相對路徑"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "move_note",
                "description": "移動或重新命名筆記。\
【操作序列】：若 from 路徑不確定 → 先 search_vault 找到來源路徑，再呼叫 move_note。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "from": {"type": "string", "description": "原始相對路徑"},
                        "to": {"type": "string", "description": "目標相對路徑（含新檔名）"}
                    },
                    "required": ["from", "to"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "show_toast",
                "description": "顯示通知訊息給使用者（適合背景任務完成後通知）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "message": {"type": "string"},
                        "kind": {"type": "string", "description": "info|success|warning|error", "enum": ["info","success","warning","error"]},
                        "duration_ms": {"type": "integer", "description": "顯示時間（毫秒），預設 3000"}
                    },
                    "required": ["message"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "ui_action",
                "description": "模擬使用者操作 UI（切換 tab、開啟搜尋等）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "description": "操作類型",
                            "enum": ["open_tab","focus_editor","open_search","new_note","open_settings","scroll_to_top"]
                        },
                        "payload": {"type": "object", "description": "額外參數（如 open_tab 需要 tab 名稱）"}
                    },
                    "required": ["action"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "reflect_on_skills",
                "description": "查看所有技能規範的觸發命中率，供 agent 自我調優",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_skills",
                "description": "當使用者的請求沒有自動觸發技能時，主動搜尋語意相似的技能規範。\
將你對使用者意圖的理解概括為簡短的 use_ask（標準化意圖，非原文）。\
例如：使用者說「今天台北天氣如何」→ use_ask 為「查詢天氣」；「幫我 Google 一下新聞」→ use_ask 為「搜尋網路新聞」。\
找到匹配技能後，請依照技能的 behavior 執行任務。\
若沒有匹配技能，直接回應使用者即可。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "use_ask": {
                            "type": "string",
                            "description": "使用者意圖的標準化概括（簡短、通用），用於語意搜尋技能庫。例如「查詢天氣」、「搜尋新聞」、「整理筆記」"
                        }
                    },
                    "required": ["use_ask"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_web",
                "description": "搜尋網路（使用 Brave Search），取得即時資訊",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "搜尋關鍵字"}
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "schedule_task",
                "description": "排程一個任務，在指定時間執行（可設定重複間隔）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "description": {"type": "string", "description": "任務描述（到時會顯示通知）"},
                        "run_at": {"type": "string", "description": "執行時間，ISO 8601 格式（如 2026-03-21T09:00:00+08:00）"},
                        "repeat_interval_seconds": {"type": "integer", "description": "重複間隔秒數，0 或省略表示只執行一次"}
                    },
                    "required": ["description", "run_at"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "compress_to_knowledge",
                "description": "主動將對話中的重要洞見、結論或知識儲存到 Vault 的 knowledge/ 資料夾。\
當對話產生了值得長期保存的見解時，主動呼叫此工具（不需等使用者要求）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "知識標題（簡潔，不超過 30 字）"},
                        "content": {"type": "string", "description": "要儲存的知識內容（Markdown 格式）"},
                        "tags": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "標籤（如 ['ai', 'productivity']），可選"
                        }
                    },
                    "required": ["title", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "update_note_frontmatter",
                "description": "局部更新筆記的 YAML frontmatter 欄位，不覆蓋正文內容。\
適合只更新 tags、status、priority 等屬性而不想修改筆記正文時使用。\
【操作序列】：若路徑不確定 → 先 search_vault 取得精確路徑，再呼叫本工具。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "筆記相對路徑（含 .md）"},
                        "fields": {
                            "type": "object",
                            "description": "要更新的欄位（鍵值對），例如 {\"status\": \"done\", \"tags\": [\"project\", \"done\"]}"
                        }
                    },
                    "required": ["path", "fields"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "find_similar_notes",
                "description": "用語意向量搜尋找出與查詢最相似的筆記。\
適合探索相關主題、查找知識重複、或發現潛在關聯時使用。\
與 search_vault 不同：search_vault 做關鍵字全文搜索；find_similar_notes 做向量語意相似度搜索。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "語意搜尋查詢（可以是句子或概念描述）"},
                        "limit": {"type": "number", "description": "返回結果數量（預設 5，最多 20）"}
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "summarize_note_collection",
                "description": "批次讀取多篇指定筆記，並由 LLM 生成整合摘要。\
適合需要對特定筆記集合做深度分析或總結時使用。\
【操作序列】：先用 search_vault 或 list_notes_in_folder 取得路徑列表，再呼叫本工具。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "筆記路徑陣列（相對路徑）"
                        },
                        "query": {"type": "string", "description": "摘要的聚焦重點（可選），例如「主要結論」、「行動項目」"}
                    },
                    "required": ["paths"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "distill_preferences",
                "description": "分析過去對話記憶，萃取使用者的工作習慣、偏好模式與常見需求。\
適合使用者詢問「你了解我的習慣嗎」或需要個人化建議前的準備步驟。",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_note_backlinks",
                "description": "查詢哪些筆記連結至指定筆記（反向連結）。\
用於了解知識圖譜中的關聯性，或找出引用某篇筆記的所有來源。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "目標筆記的相對路徑（含 .md）"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_vault_stats",
                "description": "取得知識庫的整體統計資料：筆記總數、資料夾數、總字數、最近修改的筆記。\
適合使用者詢問知識庫概況，或需要對知識庫健康度做評估時使用。",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_by_tag",
                "description": "以 frontmatter tag 標籤過濾筆記。\
比 search_vault 更精準：當使用者明確說「給我標籤是 X 的筆記」時使用此工具。\
標籤名稱不區分大小寫。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "tag": {"type": "string", "description": "要搜尋的標籤名稱（如 'project'、'done'、'reading'）"},
                        "limit": {"type": "number", "description": "最多返回幾篇（預設 50）"}
                    },
                    "required": ["tag"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "extract_action_items",
                "description": "從筆記（或整個資料夾）中提取待辦事項：包含 `- [ ]` checkbox、`TODO:`、`ACTION:`、`FIXME:` 標記。\
適合使用者說「幫我整理一下有什麼待辦」或「這個資料夾有哪些 TODO」時使用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "單一筆記的相對路徑（path 和 folder 二擇一）"},
                        "folder": {"type": "string", "description": "掃描整個資料夾的路徑（path 和 folder 二擇一）"},
                        "include_done": {"type": "boolean", "description": "是否包含已完成的 [x] 項目（預設 false）"}
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "find_orphan_notes",
                "description": "找出知識庫中沒有任何反向連結的孤立筆記（沒有任何其他筆記引用它們）。\
適合做知識庫健康診斷，找出遺忘或未整合的筆記。\
執行後建議配合 find_similar_notes + link_notes 建立連結。",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_recent_notes",
                "description": "列出最近 N 天內修改的筆記，按修改時間排序。\
適合使用者問「最近在寫什麼」或「這週修改了哪些筆記」時使用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "days": {"type": "number", "description": "往回查幾天（預設 7）"},
                        "limit": {"type": "number", "description": "最多返回幾篇（預設 20）"}
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "extract_note_links",
                "description": "取出筆記中所有出向的 [[wiki link]] 連結，用於分析知識圖譜的出向連結。\
與 get_note_backlinks（反向）相對，這是正向（此筆記連到哪裡）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "筆記相對路徑（含 .md）"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "link_notes",
                "description": "在筆記 A（from_path）中插入指向筆記 B（to_path）的 [[wiki link]]。\
若 from_path 已有 Related/Links/相關 章節則插入其中，否則自動在末尾新增 ## Related 章節。\
【操作序列】：若路徑不確定 → 先 search_vault 取得精確路徑，再呼叫本工具。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "from_path": {"type": "string", "description": "要插入連結的筆記路徑（被修改方）"},
                        "to_path": {"type": "string", "description": "要被連結到的目標筆記路徑"},
                        "section": {"type": "string", "description": "插入到指定章節名稱（可選，如 'Related'、'See Also'）"}
                    },
                    "required": ["from_path", "to_path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "generate_moc",
                "description": "為指定資料夾自動生成 Map of Contents（MOC）索引筆記。\
輸出包含資料夾內所有筆記的 [[wiki link]] 清單，按子資料夾分組。\
預設輸出至 {folder}/index.md，也可指定 output_path。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "folder": {"type": "string", "description": "要生成 MOC 的資料夾路徑（如 'projects' 或 'notes/2026'）"},
                        "output_path": {"type": "string", "description": "MOC 筆記的輸出路徑（可選，預設 {folder}/index.md）"}
                    },
                    "required": ["folder"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "live_respond",
                "description": "【必須最後呼叫】語音對話的結構化回應工具。\
完成所有資訊蒐集或操作後，必須呼叫此工具輸出最終回覆。\
speech 欄位的內容會被 TTS 朗讀，必須是自然口語、不含 Markdown。\
action 決定前端行為：\
none=只 TTS；show_results=顯示筆記清單；open_note=在編輯器開啟筆記；\
open_tab=切換頁籤；show_error=顯示錯誤卡片。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "speech": {
                            "type": "string",
                            "description": "TTS 朗讀文字，必須是口語化繁體中文（或依語言設定），2-3 句以內，不含 Markdown 或列點"
                        },
                        "action": {
                            "type": "string",
                            "enum": ["none", "show_results", "open_note", "open_tab", "show_error"],
                            "description": "前端動作類型"
                        },
                        "content": {
                            "type": "string",
                            "description": "action=show_results 時顯示在畫面上的詳細內容（網頁摘要、筆記內容等）；speech 只說短句，詳細資訊放這裡"
                        },
                        "paths": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "action=show_results 時的筆記路徑列表（vault 筆記用，與 content 擇一或並用）"
                        },
                        "path": {
                            "type": "string",
                            "description": "action=open_note 時的單一筆記路徑"
                        },
                        "tab": {
                            "type": "string",
                            "description": "action=open_tab 時的頁籤名稱（settings/trash/agents/skills）"
                        },
                        "error": {
                            "type": "string",
                            "description": "action=show_error 時的錯誤訊息"
                        }
                    },
                    "required": ["speech", "action"]
                }
            }
        }
    ])
}

// ── Tool Registry 工具 ─────────────────────────────────────────────────────

/// 余弦相似度（兩向量長度不同或為空時回傳 0.0）
#[allow(dead_code)]
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 { return 0.0; }
    dot / (norm_a * norm_b)
}

/// 從 vault_tools() 中過濾出指定名稱的工具子集。
/// plan_announce 永遠包含（寫入確認機制必需）。
fn filter_vault_tools_by_names(names: &[String]) -> serde_json::Value {
    const ALWAYS_INCLUDE: &[&str] = &["plan_announce"];
    let name_set: std::collections::HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    let filtered: Vec<serde_json::Value> = vault_tools()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| {
            let n = t["function"]["name"].as_str().unwrap_or("");
            name_set.contains(n) || ALWAYS_INCLUDE.contains(&n)
        })
        .collect();
    serde_json::Value::Array(filtered)
}

/// 驗證相對路徑安全性（防止路徑穿越），返回絕對路徑
pub(crate) fn resolve_vault_path(rel_path: &str, vault_path: &str) -> Result<PathBuf, String> {
    if rel_path.contains("..") {
        return Err("不允許路徑穿越（..）".to_string());
    }
    let abs = PathBuf::from(vault_path).join(rel_path);
    if abs.starts_with(vault_path) {
        Ok(abs)
    } else {
        Err("路徑超出 Vault 範圍".to_string())
    }
}

/// 列出指定資料夾的子資料夾和 .md 筆記（單層）
pub(crate) fn tool_list_structure(rel_path: &str, vault_path: &str) -> String {
    let abs_path = if rel_path.is_empty() {
        PathBuf::from(vault_path)
    } else {
        match resolve_vault_path(rel_path, vault_path) {
            Ok(p) => p,
            Err(e) => return e,
        }
    };
    if !abs_path.is_dir() {
        return format!("路徑不存在或不是資料夾：{}", rel_path);
    }
    let mut folders: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&abs_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if entry.path().is_dir() {
                folders.push(format!("[📁] {}", name));
            } else if name.ends_with(".md") {
                notes.push(format!("[📄] {}", name));
            }
        }
    }
    folders.sort();
    notes.sort();
    let label = if rel_path.is_empty() { "根目錄".to_string() } else { rel_path.to_string() };
    let mut lines = vec![format!("📂 {} 的內容：", label)];
    lines.extend(folders);
    lines.extend(notes);
    if lines.len() == 1 {
        lines.push("（空）".to_string());
    }
    lines.join("\n")
}

/// 讀取筆記內容（最多 6000 字元）
pub(crate) fn tool_read_note(rel_path: &str, vault_path: &str) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::read_to_string(&abs_path) {
        Ok(content) => {
            if content.len() > 6000 {
                // Snap to a valid char boundary — CJK chars are 3 bytes each,
                // so the raw byte index 6000 can land mid-character.
                let mut end = 6000usize;
                while end > 0 && !content.is_char_boundary(end) { end -= 1; }
                format!("{}\n\n[…內容過長，已截斷至約 6000 字元]", &content[..end])
            } else {
                content
            }
        }
        Err(e) => format!("讀取失敗：{}", e),
    }
}

// ── FTS 查詢清洗 ──────────────────────────────────────────────────────────────

/// 去除口語指令詞，只保留核心主題詞
/// 例：「幫我找筆記內 飲料」→「飲料」
#[allow(dead_code)]
fn clean_fts_query(query: &str) -> String {
    // 前綴指令詞（依長度降序，避免短詞先匹配）
    const PREFIXES: &[&str] = &[
        "請幫我搜尋", "請幫我搜索", "請幫我查找", "請幫我找",
        "幫我搜尋", "幫我搜索", "幫我查找", "幫我找",
        "幫我", "請找", "請搜尋", "請搜索", "請查",
        "找一下", "查一下", "搜尋一下", "搜索一下",
        "搜尋", "搜索", "查找", "找找",
        "在筆記中", "在vault中", "在Vault中",
        "筆記內", "筆記裡", "筆記中",
        "vault內", "vault裡", "Vault內", "Vault裡",
        "裡面有", "裡頭有",
    ];
    // 後綴雜訊詞
    const SUFFIXES: &[&str] = &[
        "的筆記", "的資料", "的記錄", "的內容", "的相關筆記",
        "相關的筆記", "相關的資料",
    ];
    // 常見助詞/連接詞（整詞清除）
    const STOPWORDS: &[&str] = &["的", "之", "與", "和", "或", "及"];

    let mut q = query.trim().to_string();

    // 反覆剝除前綴，直到無法再匹配
    loop {
        let before = q.clone();
        for &p in PREFIXES {
            if q.starts_with(p) {
                q = q[p.len()..].trim().to_string();
                break;
            }
        }
        if q == before {
            break;
        }
    }

    // 反覆剝除後綴
    loop {
        let before = q.clone();
        for &s in SUFFIXES {
            if q.ends_with(s) {
                let end = q.len() - s.len();
                q = q[..end].trim().to_string();
                break;
            }
        }
        if q == before {
            break;
        }
    }

    // 去除純助詞開頭（如「的奶茶」→「奶茶」）
    for &w in STOPWORDS {
        if q.starts_with(w) && q.len() > w.len() {
            q = q[w.len()..].trim().to_string();
        }
    }

    if q.is_empty() {
        query.trim().to_string() // 若清洗後為空，退回原始 query
    } else {
        q
    }
}

// ── 數值比較過濾 ─────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone)]
enum Comparison {
    LessThan(f64),
    LessThanOrEqual(f64),
    GreaterThan(f64),
    GreaterThanOrEqual(f64),
    Equal(f64),
    About(f64), // ±15%
}

#[allow(dead_code)]
impl Comparison {
    fn matches(&self, value: f64) -> bool {
        match self {
            Self::LessThan(v) => value < *v,
            Self::LessThanOrEqual(v) => value <= *v,
            Self::GreaterThan(v) => value > *v,
            Self::GreaterThanOrEqual(v) => value >= *v,
            Self::Equal(v) => (value - v).abs() < 0.01,
            Self::About(v) => (value - v).abs() <= v * 0.15,
        }
    }
    fn label(&self) -> String {
        match self {
            Self::LessThan(v) => format!("< {}", v),
            Self::LessThanOrEqual(v) => format!("≤ {}", v),
            Self::GreaterThan(v) => format!("> {}", v),
            Self::GreaterThanOrEqual(v) => format!("≥ {}", v),
            Self::Equal(v) => format!("= {}", v),
            Self::About(v) => format!("≈ {}", v),
        }
    }
}

/// 從查詢字串中解析比較詞 + 數字，返回 (比較條件, 去掉比較部分後的搜索詞)
#[allow(dead_code)]
fn parse_comparison(query: &str) -> (Option<Comparison>, String) {
    // 有序列表：長詞優先（避免「不超過」被「超過」先匹配）
    let keywords: &[(&str, &str)] = &[
        ("不超過", "lte"), ("不高於", "lte"), ("不大於", "lte"), ("至多", "lte"), ("最多", "lte"),
        ("不低於", "gte"), ("不小於", "gte"), ("至少", "gte"), ("最少", "gte"),
        ("低於", "lt"),  ("小於", "lt"),  ("少於", "lt"),  ("未達", "lt"),  ("不足", "lt"),
        ("高於", "gt"),  ("大於", "gt"),  ("多於", "gt"),  ("超過", "gt"),
        ("等於", "eq"),  ("剛好", "eq"),  ("恰好", "eq"),  ("正好", "eq"),
        ("大約", "about"), ("約為", "about"), ("大概", "about"),
        ("差不多", "about"), ("接近", "about"), ("約莫", "about"), ("左右", "about"),
        ("約", "about"),
    ];
    let units = ["元", "塊", "分", "度", "克", "公克", "公斤", "公升", "毫升",
                 "ml", "ML", "kg", "KG", "g", "G", "L", "km", "KM", "m", "M"];

    for &(word, kind) in keywords {
        if let Some(pos) = query.find(word) {
            let after = query[pos + word.len()..].trim_start();
            // 提取數字（整數或小數）
            let num_end = after
                .find(|c: char| !c.is_ascii_digit() && c != '.')
                .unwrap_or(after.len());
            if num_end == 0 {
                continue;
            }
            if let Ok(val) = after[..num_end].parse::<f64>() {
                let cmp = match kind {
                    "lt"    => Comparison::LessThan(val),
                    "lte"   => Comparison::LessThanOrEqual(val),
                    "gt"    => Comparison::GreaterThan(val),
                    "gte"   => Comparison::GreaterThanOrEqual(val),
                    "eq"    => Comparison::Equal(val),
                    _       => Comparison::About(val),
                };
                let before = query[..pos].trim();
                let rest = &after[num_end..];
                // 跳過單位
                let unit_skip = units
                    .iter()
                    .find(|&&u| rest.starts_with(u))
                    .map(|u| u.len())
                    .unwrap_or(0);
                // 去除助詞「的」「之」
                let after_unit = rest[unit_skip..]
                    .trim_start_matches('的')
                    .trim_start_matches('之')
                    .trim();
                let remaining = match (before.is_empty(), after_unit.is_empty()) {
                    (true,  true)  => String::new(),
                    (true,  false) => after_unit.to_string(),
                    (false, true)  => before.to_string(),
                    (false, false) => format!("{} {}", before, after_unit),
                };
                return (Some(cmp), remaining);
            }
        }
    }
    (None, query.to_string())
}

/// 從文字內容中找出含有數字且符合比較條件的行
#[allow(dead_code)]
fn filter_lines_by_comparison(content: &str, cmp: &Comparison) -> Vec<String> {
    let mut matched = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // 掃描行內所有數字（含小數點）
        let bytes = trimmed.as_bytes();
        let mut i = 0;
        let mut found = false;
        while i < bytes.len() && !found {
            if bytes[i].is_ascii_digit() {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                if let Ok(s) = std::str::from_utf8(&bytes[start..i]) {
                    if let Ok(val) = s.parse::<f64>() {
                        if cmp.matches(val) {
                            matched.push(trimmed.to_string());
                            found = true;
                        }
                    }
                }
            } else {
                i += 1;
            }
        }
    }
    matched
}

// ── Frontmatter helpers ────────────────────────────────────────────────────

/// Inject `status: draft` + `created_by: ai` into frontmatter if no `status` field yet.
/// If content already has `status:`, leave it unchanged.
fn inject_ai_frontmatter(content: &str) -> String {
    let after = if content.starts_with("---\r\n") {
        5
    } else if content.starts_with("---\n") {
        4
    } else {
        // No frontmatter — create one
        return format!("---\nstatus: draft\ncreated_by: ai\n---\n\n{}", content);
    };
    if let Some(end_offset) = content[after..].find("\n---") {
        let fm = &content[after..after + end_offset];
        if fm.lines().any(|l| l.trim_start().starts_with("status:")) {
            return content.to_string(); // Already has status — don't touch
        }
        let rest = &content[after + end_offset..]; // starts with "\n---..."
        format!("---\nstatus: draft\ncreated_by: ai\n{}{}", fm, rest)
    } else {
        format!("---\nstatus: draft\ncreated_by: ai\n---\n\n{}", content)
    }
}

/// Set (or insert) a single key in frontmatter. Creates frontmatter if absent.
pub(crate) fn set_frontmatter_key(content: &str, key: &str, value: &str) -> String {
    let prefix = format!("{}:", key);
    let new_line = format!("{}: {}", key, value);
    let after = if content.starts_with("---\r\n") {
        5
    } else if content.starts_with("---\n") {
        4
    } else {
        return format!("---\n{}: {}\n---\n\n{}", key, value, content);
    };
    if let Some(end_offset) = content[after..].find("\n---") {
        let fm = &content[after..after + end_offset];
        let rest = &content[after + end_offset..];
        let lines: Vec<&str> = fm.lines().collect();
        let idx = lines.iter().position(|l| l.trim_start().starts_with(&prefix));
        let new_fm = if let Some(i) = idx {
            let mut v = lines.clone();
            v[i] = &new_line;
            v.join("\n")
        } else {
            format!("{}\n{}", new_line, fm)
        };
        format!("---\n{}{}", new_fm, rest)
    } else {
        format!("---\n{}: {}\n---\n\n{}", key, value, content)
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// 建立新筆記（自動建立父資料夾）
pub(crate) async fn tool_create_note(
    rel_path: &str,
    content: &str,
    vault_path: &str,
    _db_ctx: Option<()>,
) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Some(parent) = abs_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let final_content = inject_ai_frontmatter(content);
    if let Err(e) = tokio::fs::write(&abs_path, &final_content).await {
        return format!("建立失敗：{}", e);
    }
    format!("✅ 已建立筆記：{}", rel_path)
}

/// 更新現有筆記（覆寫全文）
pub(crate) async fn tool_update_note(
    rel_path: &str,
    content: &str,
    vault_path: &str,
    _db_ctx: Option<()>,
) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let final_content = inject_ai_frontmatter(content);
    if let Err(e) = tokio::fs::write(&abs_path, &final_content).await {
        return format!("更新失敗：{}", e);
    }
    format!("✅ 已更新筆記：{}", rel_path)
}

/// 建立資料夾
pub(crate) async fn tool_create_folder(rel_path: &str, vault_path: &str) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match tokio::fs::create_dir_all(&abs_path).await {
        Ok(_) => format!("✅ 已建立資料夾：{}", rel_path),
        Err(e) => format!("建立失敗：{}", e),
    }
}

/// 設定筆記 status frontmatter（draft | verified | deprecated）
/// 對 vault 筆記：讀寫磁碟 + 更新 notes table + chunks。
/// 對 KB import 頁面（無磁碟檔案）：讀寫 import_pages.content_md + 更新 chunks。
#[tauri::command]
pub async fn set_note_status(
    state: State<'_, AppState>,
    path: String,
    status: String,
) -> Result<(), AppError> {
    if !matches!(status.as_str(), "draft" | "verified" | "deprecated") {
        return Err(AppError::AI(format!("Invalid status: {}", status)));
    }
    let vault_id = state.get_vault_id().await?;
    let vault_path = state.get_vault_path().await;

    // Try reading from disk first; fall back to import_pages.content_md for KB-only pages
    let abs = if !vault_path.is_empty() {
        Some(std::path::Path::new(&vault_path).join(&path))
    } else {
        None
    };

    let on_disk = abs.as_ref().map(|p| p.exists()).unwrap_or(false);

    let _new_content: String = if on_disk {
        let abs_path = abs.as_ref().unwrap();
        let content = tokio::fs::read_to_string(abs_path).await
            .map_err(|e| AppError::AI(format!("Read failed: {}", e)))?;
        let updated = set_frontmatter_key(&content, "status", &status);
        tokio::fs::write(abs_path, &updated).await
            .map_err(|e| AppError::AI(format!("Write failed: {}", e)))?;
        // Sync updated note to daemon (no file watcher in daemon)
        {
            let token = state.get_auth_token().await;
            let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
            let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
                &state.http_client,
                &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
                &serde_json::json!({"path": path, "content": updated.clone()}),
                tok,
            ).await;
        }
        updated
    } else {
        // KB import page — no DB, just return status frontmatter
        format!("---\nstatus: {}\n---\n\n", status)
    };
    // chunk upsert handled by daemon on /notes upsert above
    Ok(())
}

/// 判斷工具是否為寫入操作（需要使用者確認）
fn is_write_tool(name: &str) -> bool {
    matches!(name, "create_note" | "update_note" | "create_folder")
}

/// 筆記路徑：若不以 .md 結尾則自動補上
fn ensure_md(path: &str) -> String {
    if path.is_empty() || path.ends_with(".md") {
        path.to_string()
    } else {
        format!("{}.md", path)
    }
}

/// 從 StreamResult 提取所有 tool calls（native 格式優先，fallback 文字格式）
/// 回傳 Vec<(tool_id, tool_name, tool_args)>，空 Vec 表示純文字回覆
pub(crate) fn detect_tool_calls(
    result: &StreamResult,
) -> Vec<(String, String, serde_json::Value)> {
    // Native OpenAI tool_calls 格式（可能多個）
    if result.finish_reason == "tool_calls" && !result.tool_call_chunks.is_empty() {
        return result.tool_call_chunks.iter().map(|acc| {
            let args: serde_json::Value =
                serde_json::from_str(&acc.arguments).unwrap_or(serde_json::json!({}));
            (acc.id.clone(), acc.name.clone(), args)
        }).collect();
    }
    // 文字格式 fallback <tool_call>...</tool_call>（可能多個）
    if result.full_text.contains("<tool_call>") {
        return parse_text_tool_calls(&result.full_text).into_iter().map(|call| {
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
            let args: serde_json::Value =
                serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
            (String::new(), name, args)
        }).collect();
    }
    vec![]
}

/// 分派工具調用到對應的實作函式
async fn execute_vault_tool(
    name: &str,
    args: &serde_json::Value,
    vault_path: &str,
    app: &AppHandle,
) -> String {
    if vault_path.is_empty() {
        return "Vault 未設定，無法執行 Vault 操作".to_string();
    }
    match name {
        "search_vault" => {
            let query = args["query"].as_str().unwrap_or("");
            let state = app.state::<crate::state::AppState>();
            let token = state.get_auth_token().await;
            let tok = if token.is_empty() { None } else { Some(token.as_str()) };
            let vault_id = state.get_vault_uuid().await;
            if vault_id.is_empty() || query.is_empty() {
                return "搜尋失敗：未設定 Vault 或查詢為空".to_string();
            }
            let url = format!("/vaults/{}/search?q={}", urlencoding::encode(&vault_id), urlencoding::encode(query));
            match crate::api_client::daemon_get::<serde_json::Value>(&state.http_client, &url, tok).await {
                Ok(results) => {
                    let arr = results.as_array().cloned().unwrap_or_default();
                    if arr.is_empty() {
                        format!("搜尋「{}」：找不到相關筆記。", query)
                    } else {
                        let lines: Vec<String> = arr.iter().take(5).map(|r| {
                            format!("- **{}** ({})", r["title"].as_str().unwrap_or(""), r["path"].as_str().unwrap_or(""))
                        }).collect();
                        format!("搜尋「{}」結果：\n{}", query, lines.join("\n"))
                    }
                }
                Err(_) => format!("搜尋「{}」失敗，請稍後再試。", query),
            }
        }
        "list_structure" => {
            let path = args["path"].as_str().unwrap_or("");
            tool_list_structure(path, vault_path)
        }
        "read_note" => {
            let path = ensure_md(args["path"].as_str().unwrap_or(""));
            tool_read_note(&path, vault_path)
        }
        "create_note" => {
            let path = ensure_md(args["path"].as_str().unwrap_or(""));
            let content = args["content"].as_str().unwrap_or("");
            let result = tool_create_note(&path, content, vault_path, None).await;
            // Sync to daemon after filesystem write
            {
                let st = app.state::<crate::state::AppState>();
                let vault_id = st.get_vault_uuid().await;
                let token = st.get_auth_token().await;
                let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
                if !vault_id.is_empty() && !path.is_empty() {
                    let abs = std::path::PathBuf::from(vault_path).join(&path);
                    if let Ok(c) = tokio::fs::read_to_string(&abs).await {
                        let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
                            &st.http_client,
                            &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
                            &serde_json::json!({"path": path, "content": c}),
                            tok,
                        ).await;
                    }
                }
            }
            result
        }
        "update_note" => {
            let path = ensure_md(args["path"].as_str().unwrap_or(""));
            let content = args["content"].as_str().unwrap_or("");
            let result = tool_update_note(&path, content, vault_path, None).await;
            // Sync to daemon after filesystem write
            {
                let st = app.state::<crate::state::AppState>();
                let vault_id = st.get_vault_uuid().await;
                let token = st.get_auth_token().await;
                let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
                if !vault_id.is_empty() && !path.is_empty() {
                    let abs = std::path::PathBuf::from(vault_path).join(&path);
                    if let Ok(c) = tokio::fs::read_to_string(&abs).await {
                        let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
                            &st.http_client,
                            &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
                            &serde_json::json!({"path": path, "content": c}),
                            tok,
                        ).await;
                    }
                }
            }
            result
        }
        "create_folder" => {
            let path = args["path"].as_str().unwrap_or("");
            let result = tool_create_folder(path, vault_path).await;
            // Trigger vault rescan so daemon indexes any new structure
            {
                let st = app.state::<crate::state::AppState>();
                let vault_id = st.get_vault_uuid().await;
                let token = st.get_auth_token().await;
                let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
                if !vault_id.is_empty() {
                    let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
                        &st.http_client,
                        &format!("/vaults/{}/scan", urlencoding::encode(&vault_id)),
                        &serde_json::json!({}),
                        tok,
                    ).await;
                }
            }
            result
        }
        "query_memory" => {
            let keywords_val = &args["keywords"];
            let keywords: Vec<String> = if let Some(arr) = keywords_val.as_array() {
                arr.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect()
            } else if let Some(s) = keywords_val.as_str() {
                vec![s.to_string()]
            } else {
                vec![]
            };
            let limit = args["limit"].as_u64().unwrap_or(5).min(20);
            let state = app.state::<crate::state::AppState>();
            let token = state.get_auth_token().await;
            let tok = if token.is_empty() { None } else { Some(token.as_str()) };
            let vault_id = state.get_vault_uuid().await;
            if vault_id.is_empty() {
                return "記憶查詢失敗：未設定 Vault".to_string();
            }
            let kw_param = urlencoding::encode(&keywords.join(",")).to_string();
            // Fetch more candidates than needed for potential rerank
            let fetch_limit = (limit * 3).min(30);
            let url = format!("/vaults/{}/memory/query?keywords={}&limit={}", urlencoding::encode(&vault_id), kw_param, fetch_limit);
            match crate::api_client::daemon_get::<serde_json::Value>(&state.http_client, &url, tok).await {
                Ok(results) => {
                    let mut arr = results.as_array().cloned().unwrap_or_default();
                    if arr.is_empty() {
                        "記憶查詢：找不到相關記憶。".to_string()
                    } else {
                        // Cosine rerank if embedding server is available and facts have embeddings
                        let emb_url = {
                            let port = *state.llama_actual_port.lock().await;
                            port.map(|p| format!("http://127.0.0.1:{}", p))
                        };
                        if let Some(ref url) = emb_url {
                            let query_text = if keywords.is_empty() { String::new() } else { keywords.join(" ") };
                            if !query_text.is_empty() {
                                let query_vec = crate::commands::server::get_embedding(&state.http_client, url, &query_text).await;
                                if !query_vec.is_empty() {
                                    arr.sort_by(|a, b| {
                                        let sim_a = a["embedding"].as_str()
                                            .and_then(|s| serde_json::from_str::<Vec<f32>>(s).ok())
                                            .map(|v| cosine_similarity(&query_vec, &v))
                                            .unwrap_or(0.0);
                                        let sim_b = b["embedding"].as_str()
                                            .and_then(|s| serde_json::from_str::<Vec<f32>>(s).ok())
                                            .map(|v| cosine_similarity(&query_vec, &v))
                                            .unwrap_or(0.0);
                                        sim_b.partial_cmp(&sim_a).unwrap_or(std::cmp::Ordering::Equal)
                                    });
                                }
                            }
                        }
                        let lines: Vec<String> = arr.iter().take(limit as usize).map(|r| {
                            let cat = r["category"].as_str().unwrap_or("general");
                            let content = r["content"].as_str().unwrap_or("");
                            format!("- [{}] {}", cat, content)
                        }).collect();
                        format!("記憶查詢結果：\n{}", lines.join("\n"))
                    }
                }
                Err(_) => "記憶查詢失敗，請稍後再試。".to_string(),
            }
        }
        "prefetch_memory" => {
            let context = args["context"].as_str().unwrap_or("").to_string();
            let state = app.state::<crate::state::AppState>();
            let token = state.get_auth_token().await;
            let tok = if token.is_empty() { None } else { Some(token.as_str()) };
            let vault_id = state.get_vault_uuid().await;
            if vault_id.is_empty() {
                return "記憶預取失敗：未設定 Vault".to_string();
            }
            // Extract keywords from context (simple: split whitespace, take CJK bigrams)
            let kw_param = if context.is_empty() {
                String::new()
            } else {
                urlencoding::encode(context.chars().take(60).collect::<String>().trim()).to_string()
            };
            let url = format!(
                "/vaults/{}/memory/query?keywords={}&limit=8",
                urlencoding::encode(&vault_id), kw_param
            );
            match crate::api_client::daemon_get::<serde_json::Value>(&state.http_client, &url, tok).await {
                Ok(results) => {
                    let arr = results.as_array().cloned().unwrap_or_default();
                    if arr.is_empty() {
                        String::new()
                    } else {
                        // Emit prefetched node_ids for MemoryLinksView highlight
                        let node_ids: Vec<String> = arr.iter()
                            .filter_map(|r| r["fact_id"].as_str())
                            .map(|fid| format!("memory:{}:{}", vault_id, fid))
                            .collect();
                        if !node_ids.is_empty() {
                            let _ = app.emit("memory:prefetched", serde_json::json!({
                                "node_ids": node_ids,
                                "source": "live_chat"
                            }));
                        }
                        let lines: Vec<String> = arr.iter().map(|r| {
                            let cat = r["category"].as_str().unwrap_or("general");
                            let content = r["content"].as_str().unwrap_or("");
                            format!("[{}] {}", cat, content)
                        }).collect();
                        format!("## 相關記憶\n{}", lines.join("\n"))
                    }
                }
                Err(_) => String::new(),
            }
        }
        "think" => {
            let thought = args["thought"].as_str().unwrap_or("").trim().to_string();
            if !thought.is_empty() {
                let _ = app.emit("live_chat:thinking", &thought);
            }
            String::new()
        }
        "find_related" => {
            let path = ensure_md(args["path"].as_str().unwrap_or(""));
            if path.is_empty() {
                return "請提供筆記路徑".to_string();
            }
            let depth = args["depth"].as_u64().unwrap_or(1).min(2);
            let limit = args["limit"].as_u64().unwrap_or(10).min(30);
            let state = app.state::<crate::state::AppState>();
            let token = state.get_auth_token().await;
            let tok = if token.is_empty() { None } else { Some(token.as_str()) };
            let vault_id = state.get_vault_uuid().await;
            if vault_id.is_empty() {
                return "find_related 失敗：未設定 Vault".to_string();
            }
            let url = format!(
                "/vaults/{}/graph/related?path={}&depth={}&limit={}",
                urlencoding::encode(&vault_id),
                urlencoding::encode(&path),
                depth,
                limit
            );
            match crate::api_client::daemon_get::<serde_json::Value>(&state.http_client, &url, tok).await {
                Ok(data) => {
                    let nodes = data["nodes"].as_array().cloned().unwrap_or_default();
                    if nodes.is_empty() {
                        format!("「{}」在知識圖譜中沒有相關連結的筆記。", path)
                    } else {
                        let lines: Vec<String> = nodes.iter().map(|n| {
                            let label = n["label"].as_str().unwrap_or("(無標題)");
                            let fp = n["file_path"].as_str().unwrap_or("");
                            let rel = n["relation"].as_str().unwrap_or("link");
                            format!("- [{}] {} ({})", rel, label, fp)
                        }).collect();
                        format!("「{}」的相關筆記（深度 {}）：\n{}", path, depth, lines.join("\n"))
                    }
                }
                Err(e) => format!("find_related 失敗：{}", e),
            }
        }
        "open_note" => {
            let path = ensure_md(args["path"].as_str().unwrap_or(""));
            if path.is_empty() {
                return "請提供筆記路徑".to_string();
            }
            // 前端 openNote() 期望 relative path，直接傳入
            let _ = app.emit("ui:open_note", &path);
            format!("✅ 已打開筆記：{}", path)
        }
        _ => format!("未知工具：{}", name),
    }
}

/// 前端確認/拒絕寫入工具（stream_chat 等待此命令後繼續執行）
#[tauri::command]
pub async fn confirm_write_tool(
    state: State<'_, AppState>,
    approved: bool,
) -> Result<(), AppError> {
    let tx = state.write_confirm_tx.lock().await.take();
    if let Some(tx) = tx {
        let _ = tx.send(approved);
    }
    Ok(())
}

// ── Pipeline 型別（用於 run_tool_pipeline） ───────────────────────────────

#[derive(serde::Deserialize)]
pub struct PipelineStep {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
    /// 前置步驟 ID 列表（相依必須先執行完畢）
    pub deps: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct PipelineStepResult {
    pub id: String,
    pub name: String,
    pub ok: bool,
    /// true = 取消旗標觸發，此步驟未執行
    pub cancelled: bool,
    pub output: String,
    pub duration_ms: u64,
}

/// vault:changed 事件 payload（寫入工具 commit 後 emit，觸發前端 sidebar + editor 刷新）
#[derive(serde::Serialize, Clone)]
pub struct VaultChangedPayload {
    pub creates: Vec<String>,
    pub updates: Vec<String>,
}

/// 取消進行中的工具測試台 Pipeline
#[tauri::command]
pub async fn cancel_tool_test(state: State<'_, AppState>) -> Result<(), AppError> {
    state.tool_test_cancel.store(true, Ordering::Relaxed);
    Ok(())
}

/// Kahn's 演算法拓撲排序，返回 steps 陣列的執行索引順序
fn topo_sort_indices(steps: &[PipelineStep]) -> Vec<usize> {
    use std::collections::{HashMap, VecDeque};
    let id_to_idx: HashMap<&str, usize> = steps.iter().enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();
    let n = steps.len();
    let mut in_degree = vec![0usize; n];
    let mut successors: Vec<Vec<usize>> = vec![vec![]; n];

    for (i, step) in steps.iter().enumerate() {
        for dep_id in &step.deps {
            if let Some(&dep_idx) = id_to_idx.get(dep_id.as_str()) {
                in_degree[i] += 1;
                successors[dep_idx].push(i);
            }
        }
    }

    let mut queue: VecDeque<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut ordered = Vec::with_capacity(n);

    while let Some(i) = queue.pop_front() {
        ordered.push(i);
        for &j in &successors[i] {
            in_degree[j] -= 1;
            if in_degree[j] == 0 {
                queue.push_back(j);
            }
        }
    }

    // 有環路時，其餘步驟依原始順序追加
    if ordered.len() < n {
        for i in 0..n {
            if !ordered.contains(&i) {
                ordered.push(i);
            }
        }
    }

    ordered
}

/// 依 Planner ToolGraph 相容格式執行多工具 Pipeline，供 debug 測試台使用
///
/// Transaction 語意：
/// - 開始前 emit `agent:tx_debug` kind="prepare"
/// - 每步驟執行前檢查取消旗標；若已取消，剩餘步驟標記 cancelled=true 並 emit "cancel"
/// - 全部完成後 emit kind="commit"
/// - 有寫入工具時 emit `vault:changed`（觸發前端 sidebar + editor 刷新）
#[tauri::command]
pub async fn run_tool_pipeline(
    app: AppHandle,
    state: State<'_, AppState>,
    steps: Vec<PipelineStep>,
) -> Result<Vec<PipelineStepResult>, AppError> {
    // 重置取消旗標（新 pipeline 開始）
    state.tool_test_cancel.store(false, Ordering::Relaxed);

    let session_id = Uuid::new_v4().to_string();
    let vault_path = state.get_vault_path().await;

    let all_tool_names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();

    // ── Emit prepare ─────────────────────────────────────────────────────────
    let _ = app.emit("agent:tx_debug", serde_json::json!({
        "session_id": session_id,
        "kind": "prepare",
        "tools": all_tool_names,
    }));

    let order = topo_sort_indices(&steps);
    let mut results: Vec<PipelineStepResult> = Vec::with_capacity(steps.len());
    let mut executed_names: Vec<String> = Vec::new();
    let mut vault_creates: Vec<String> = Vec::new();
    let mut vault_updates: Vec<String> = Vec::new();

    for idx in order {
        let step = &steps[idx];

        // ── 取消檢查（每步執行前）─────────────────────────────────────────
        if state.tool_test_cancel.load(Ordering::Relaxed) {
            results.push(PipelineStepResult {
                id: step.id.clone(),
                name: step.name.clone(),
                ok: false,
                cancelled: true,
                output: String::new(),
                duration_ms: 0,
            });
            continue;
        }

        let start = std::time::Instant::now();
        let output = execute_vault_tool(
            &step.name,
            &step.args,
            &vault_path,
            &app,
        ).await;
        let duration_ms = start.elapsed().as_millis() as u64;

        // 記錄寫入路徑以便 commit 後 emit vault:changed
        match step.name.as_str() {
            "create_note" | "create_folder" => {
                if let Some(p) = step.args["path"].as_str() {
                    vault_creates.push(p.to_string());
                }
            }
            "update_note" => {
                if let Some(p) = step.args["path"].as_str() {
                    vault_updates.push(p.to_string());
                }
            }
            _ => {}
        }

        executed_names.push(step.name.clone());
        results.push(PipelineStepResult {
            id: step.id.clone(),
            name: step.name.clone(),
            ok: true,
            cancelled: false,
            output,
            duration_ms,
        });
    }

    // ── Emit commit 或 cancel ────────────────────────────────────────────────
    let was_cancelled = state.tool_test_cancel.load(Ordering::Relaxed);
    let kind = if was_cancelled { "cancel" } else { "commit" };
    let _ = app.emit("agent:tx_debug", serde_json::json!({
        "session_id": session_id,
        "kind": kind,
        "tools": executed_names,
    }));

    // ── vault:changed（commit 且有寫入操作）──────────────────────────────────
    if !was_cancelled && (!vault_creates.is_empty() || !vault_updates.is_empty()) {
        let _ = app.emit("vault:changed", VaultChangedPayload {
            creates: vault_creates,
            updates: vault_updates,
        });
    }

    Ok(results)
}

/// 直接測試單一 Agent 工具，供 debug 面板使用
///
/// Transaction 語意：
/// - 開始前 emit `agent:tx_debug` kind="prepare"
/// - 執行完畢 emit kind="commit"（若旗標已被取消則 emit "cancel"）
/// - 寫入工具 commit 後 emit `vault:changed`（觸發前端 sidebar + editor 刷新）
#[tauri::command]
pub async fn test_vault_tool(
    app: AppHandle,
    state: State<'_, AppState>,
    tool_name: String,
    args: serde_json::Value,
) -> Result<String, AppError> {
    // 重置取消旗標（新測試開始）
    state.tool_test_cancel.store(false, Ordering::Relaxed);

    let session_id = Uuid::new_v4().to_string();
    let vault_path = state.get_vault_path().await;

    // ── Emit prepare ─────────────────────────────────────────────────────────
    let _ = app.emit("agent:tx_debug", serde_json::json!({
        "session_id": session_id,
        "kind": "prepare",
        "tools": [tool_name.clone()],
    }));

    let result = execute_vault_tool(
        &tool_name,
        &args,
        &vault_path,
        &app,
    ).await;

    // ── Emit commit 或 cancel ────────────────────────────────────────────────
    let was_cancelled = state.tool_test_cancel.load(Ordering::Relaxed);
    let kind = if was_cancelled { "cancel" } else { "commit" };
    let _ = app.emit("agent:tx_debug", serde_json::json!({
        "session_id": session_id,
        "kind": kind,
        "tools": [tool_name.clone()],
    }));

    // ── vault:changed（commit 且為寫入工具）──────────────────────────────────
    if !was_cancelled && is_write_tool(&tool_name) {
        let path = args["path"].as_str().unwrap_or("").to_string();
        let (creates, updates) = match tool_name.as_str() {
            "update_note" => (vec![], vec![path]),
            _ => (vec![path], vec![]),
        };
        let _ = app.emit("vault:changed", VaultChangedPayload { creates, updates });
    }

    Ok(result)
}

/// 偵測回覆是否包含可重用的結構化回答框架（bottom-up skill 歸納觸發條件）
fn detect_response_framework(text: &str) -> bool {
    // 含有編號步驟（1. 2. 3. 或 ①②③）
    let has_numbered = (text.contains("1.") || text.contains("1、") || text.contains("①"))
        && (text.contains("2.") || text.contains("2、") || text.contains("②"));
    // 含有「先…再…最後」結構
    let has_sequential = (text.contains("先") && text.contains("再") && text.contains("最後"))
        || (text.contains("首先") && text.contains("接著"));
    // 含有明顯框架關鍵字
    let has_framework_kw = text.contains("步驟") || text.contains("流程") || text.contains("規範");
    // 回覆夠長（>300 字）才考慮
    text.len() > 300 && (has_numbered || has_sequential || has_framework_kw)
}

/// 工具用：根據 use_ask 語意搜尋最相似的技能規範（daemon 版）。
/// 回傳 Vec<(skill_id, title, behavior, tool_calls, need_tool_chain, tool_chain_order, injection_mode)>。
pub(crate) async fn search_skills_for_tool(
    http_client: &reqwest::Client,
    auth_token: &str,
    vault_id: &str,
    use_ask: &str,
    _emb_url: Option<&str>,
    _llama_client: &reqwest::Client,
) -> Vec<(String, String, String, Vec<String>, bool, Vec<String>, String)> {
    let tok = if auth_token.is_empty() { None } else { Some(auth_token) };
    // daemon 的 GET /vaults/:vid/skills 直接回傳 JSON array（非 {"skills":[...]}）
    let result: serde_json::Value = crate::api_client::daemon_get(
        http_client,
        &format!("/vaults/{}/skills", urlencoding::encode(vault_id)),
        tok,
    ).await.unwrap_or_else(|_| serde_json::json!([]));
    let skills = match result.as_array() {
        Some(arr) => arr.clone(),
        None => return vec![],
    };
    let use_ask_lower = use_ask.to_lowercase();
    skills.iter().filter(|s| {
        let is_active = s["is_active"].as_bool().unwrap_or(true);
        let trigger = s["trigger"].as_str().unwrap_or("").to_lowercase();
        let mode = s["injection_mode"].as_str().unwrap_or("passive");
        if !is_active { return false; }
        if mode == "active" || mode == "proactive" { return true; }
        trigger.split(['、', ',', '，']).any(|kw| {
            let kw = kw.trim();
            !kw.is_empty() && use_ask_lower.contains(kw)
        })
    }).map(|s| {
        let skill_id = s["skill_id"].as_str().unwrap_or("").to_string();
        let title = s["title"].as_str().unwrap_or("").to_string();
        let behavior = s["behavior"].as_str().unwrap_or("").to_string();
        // tool_calls 可能是 native array（新版 seed）或 JSON string（舊版 create_agent_skill）
        let tool_calls: Vec<String> = if let Some(arr) = s["tool_calls"].as_array() {
            arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
        } else if let Some(s) = s["tool_calls"].as_str() {
            serde_json::from_str::<Vec<String>>(s).unwrap_or_default()
        } else {
            vec![]
        };
        let need_tool_chain = s["need_tool_chain"].as_bool().unwrap_or(false);
        let tool_chain_order: Vec<String> = if let Some(arr) = s["tool_chain_order"].as_array() {
            arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
        } else {
            vec![]
        };
        let injection_mode = s["injection_mode"].as_str().unwrap_or("passive").to_string();
        (skill_id, title, behavior, tool_calls, need_tool_chain, tool_chain_order, injection_mode)
    }).collect()
}

/// 將 use_ask 加入指定技能的 trigger 欄位，並重新計算 trigger_embedding。
#[tauri::command]
pub async fn add_skill_trigger(
    skill_id: String,
    use_ask: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let vault_id = state.get_vault_id().await.map_err(|e| e.to_string())?;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    // GET current skill trigger from daemon
    let current_skill: serde_json::Value = crate::api_client::daemon_get(
        &state.http_client,
        &format!("/vaults/{}/skills/{}", urlencoding::encode(&vault_id), urlencoding::encode(&skill_id)),
        tok,
    ).await.unwrap_or_else(|_| serde_json::json!({}));

    let current_trigger = current_skill["trigger"].as_str().unwrap_or("").to_string();
    let new_trigger = if current_trigger.is_empty() {
        use_ask.clone()
    } else {
        format!("{}、{}", current_trigger, use_ask)
    };

    let emb_url = {
        let port = *state.embedding_actual_port.lock().await;
        port.map(|p| format!("http://127.0.0.1:{}", p))
    };

    let mut update_body = serde_json::json!({"trigger": new_trigger});

    if let Some(url) = &emb_url {
        let new_embedding: Vec<f32> = get_embedding(&state.http_client, url, &new_trigger).await;
        if !new_embedding.is_empty() {
            update_body["trigger_embedding"] = serde_json::json!(new_embedding);
        }
    }

    crate::api_client::daemon_put::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/skills/{}", urlencoding::encode(&vault_id), urlencoding::encode(&skill_id)),
        &update_body,
        tok,
    ).await.map(|_| ()).map_err(|e| e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_comparison_basic() {
        let (cmp, q) = parse_comparison("低於65元的飲料");
        assert!(matches!(cmp, Some(Comparison::LessThan(v)) if (v - 65.0).abs() < 0.01));
        assert_eq!(q, "飲料");

        let (cmp, q) = parse_comparison("高於100元的咖啡");
        assert!(matches!(cmp, Some(Comparison::GreaterThan(v)) if (v - 100.0).abs() < 0.01));
        assert_eq!(q, "咖啡");

        let (cmp, q) = parse_comparison("不超過50元的奶茶");
        assert!(matches!(cmp, Some(Comparison::LessThanOrEqual(v)) if (v - 50.0).abs() < 0.01));
        assert_eq!(q, "奶茶");

        let (cmp, q) = parse_comparison("大約30元的紅茶");
        assert!(matches!(cmp, Some(Comparison::About(v)) if (v - 30.0).abs() < 0.01));
        assert_eq!(q, "紅茶");

        let (cmp, q) = parse_comparison("至少80元的果汁");
        assert!(matches!(cmp, Some(Comparison::GreaterThanOrEqual(v)) if (v - 80.0).abs() < 0.01));
        assert_eq!(q, "果汁");
    }

    #[test]
    fn test_parse_comparison_llm_style() {
        // LLM 通常會把比較詞放前面
        let (cmp, q) = parse_comparison("飲料 低於65元");
        assert!(matches!(cmp, Some(Comparison::LessThan(v)) if (v - 65.0).abs() < 0.01));
        assert_eq!(q, "飲料");

        // 無比較詞時原樣返回
        let (cmp, q) = parse_comparison("奶茶");
        assert!(cmp.is_none());
        assert_eq!(q, "奶茶");
    }

    #[test]
    fn test_clean_fts_query() {
        // 完整口語句子 → 核心詞
        assert_eq!(clean_fts_query("幫我找筆記內低於65元的飲料"), "低於65元的飲料");
        assert_eq!(clean_fts_query("搜尋奶茶"), "奶茶");
        assert_eq!(clean_fts_query("請幫我找咖啡的筆記"), "咖啡");
        assert_eq!(clean_fts_query("在筆記中高於100元的果汁"), "高於100元的果汁");
        assert_eq!(clean_fts_query("找一下紅茶"), "紅茶");
        // 無指令詞時原樣
        assert_eq!(clean_fts_query("飲料"), "飲料");
    }

    #[test]
    fn test_full_pipeline() {
        // 模擬完整流程：口語句 → 清洗 → 解析比較 → 最終 FTS 詞
        let simulate = |raw: &str| -> (bool, String) {
            let cleaned = clean_fts_query(raw);
            let (cmp, search_query) = parse_comparison(&cleaned);
            let fts = {
                let q = if search_query.trim().is_empty() { cleaned.clone() } else { search_query.trim().to_string() };
                clean_fts_query(&q)
            };
            (cmp.is_some(), fts)
        };

        let (has_cmp, fts) = simulate("幫我找筆記內低於65元的飲料");
        assert!(has_cmp, "應解析出比較條件");
        assert_eq!(fts, "飲料");

        let (has_cmp, fts) = simulate("搜尋高於100元的咖啡");
        assert!(has_cmp);
        assert_eq!(fts, "咖啡");

        let (has_cmp, fts) = simulate("找一下奶茶");
        assert!(!has_cmp);
        assert_eq!(fts, "奶茶");
    }

    #[test]
    fn test_filter_lines_by_comparison() {
        let content = "\
珍珠奶茶 60元
抹茶拿鐵 75元
草莓奶昔 55元
黑糖鮮奶茶 65元
焦糖瑪奇朵 80元";

        let cmp = Comparison::LessThan(65.0);
        let matched = filter_lines_by_comparison(content, &cmp);
        // 60 < 65 ✓, 75 ✗, 55 < 65 ✓, 65 not < 65 ✗, 80 ✗
        assert_eq!(matched.len(), 2);
        assert!(matched[0].contains("60"));
        assert!(matched[1].contains("55"));

        let cmp = Comparison::LessThanOrEqual(65.0);
        let matched = filter_lines_by_comparison(content, &cmp);
        // 60 ✓, 55 ✓, 65 ≤ 65 ✓
        assert_eq!(matched.len(), 3);

        let cmp = Comparison::About(65.0); // ±15% → 55.25~74.75
        let matched = filter_lines_by_comparison(content, &cmp);
        // 60 ✓, 75 ✓, 55 ✗（54.25邊緣，55.25以上才算）, 65 ✓
        assert!(matched.iter().any(|l| l.contains("60")));
        assert!(matched.iter().any(|l| l.contains("65")));
    }
}
