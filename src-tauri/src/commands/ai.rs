use crate::{db::queries, error::AppError, state::AppState};
use crate::db::surreal::SurrealDb;
use crate::runtime::memory_agent::{
    parse_text_tool_calls, tool_query_memory,
};
use chrono::Local;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};
use tokio::io::AsyncReadExt;

// Re-export from sub-modules so lib.rs imports continue to work unchanged
// ─── Re-exports from sub-modules ──────────────────────────────────────────────
pub use super::server::{
    get_embedding, warmup_llama_server,
    stop_llama_server, get_llama_server_status, start_llama_server, restart_llama_server,
    warmup_embedding_server, get_embedding_server_status, check_embedding_endpoint,
    start_embedding_server, stop_embedding_server, restart_embedding_server,
};
pub(crate) use super::server::{ensure_server_running, ensure_embedding_server_running};
pub use super::external_ai::{
    stream_chat_external, process_with_llm,
    call_external_ai_tool, ExtAiConfig,
};
pub(crate) use super::external_ai::{
    call_external_ai_via_db, read_api_key, get_cached_setting,
};
pub use super::memory::{
    add_memory_rule, get_memory_rules, delete_memory_rule,
    save_memory_session, query_memory,
    distill_preferences, extract_memory_facts,
    rate_response, get_conversation_ratings, analyze_tool_patterns,
    MemoryRuleEntry, MemoryResult,
};
pub(crate) use super::memory::retrieve_relevant_facts;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}
/// 呼叫 LLM（非串流）根據 user_ask 生成 agent 規格 JSON。
/// 回傳 (name, description, trigger, tool_names)；任何錯誤 fallback 至 raw input。
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
可用 tool_names：search_vault, read_note, open_note, list_structure, create_note, update_note, create_folder, query_memory, call_external_ai, list_recent_conversations\n\
選擇原則：\n\
- 筆記查詢/搜尋 → [\"search_vault\"]\n\
- 筆記打開（讓使用者在編輯器中查看）→ [\"search_vault\",\"open_note\"]\n\
- 筆記閱讀/分析內容 → [\"search_vault\",\"read_note\"]\n\
- 筆記寫入/更新 → [\"create_note\",\"update_note\",\"create_folder\"]\n\
- 外部資訊/網路查詢 → [\"call_external_ai\"]\n\
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

/// 大型 read_note 結果的並行分段摘要。
/// 從 chunks 表取出該筆記的 heading-level 切段，分批並行呼叫非串流 LLM，
/// 依使用者查詢提取要點後合併回傳。
/// chunks 不存在時回傳 None（呼叫方 fallback 至截斷）。
pub(crate) async fn parallel_chunk_summarize(
    db: &SurrealDb,
    vault_id: &str,
    file_path: &str,
    user_query: &str,
    client: &reqwest::Client,
    base_url: &str,
) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct ChunkRow { section: String, content: String }

    let mut res = db
        .query("SELECT section, content FROM chunks WHERE vault_id = $vid AND file_path = $fp ORDER BY id")
        .bind(("vid", vault_id.to_string()))
        .bind(("fp", file_path.to_string()))
        .await.ok()?;
    let rows: Vec<ChunkRow> = res.take(0).ok()?;
    if rows.is_empty() {
        return None;
    }

    // 批次分組：每批最多 2500 chars
    const BATCH_CHARS: usize = 2500;
    let mut batches: Vec<String> = Vec::new();
    let mut current = String::new();
    for row in rows {
        let chunk_text = if row.section.is_empty() {
            row.content.clone()
        } else {
            format!("## {}\n{}", row.section, row.content)
        };
        if !current.is_empty() && current.chars().count() + chunk_text.chars().count() > BATCH_CHARS {
            batches.push(std::mem::take(&mut current));
        }
        if !current.is_empty() { current.push('\n'); }
        current.push_str(&chunk_text);
    }
    if !current.is_empty() { batches.push(current); }

    let file_label = file_path.split('/').last().unwrap_or(file_path).trim_end_matches(".md").to_string();

    // 並行呼叫非串流 LLM 摘要各批次
    let futs: Vec<_> = batches.into_iter().enumerate().map(|(i, chunk)| {
        let client = client.clone();
        let base_url = base_url.to_string();
        let query = user_query.to_string();
        let label = file_label.clone();
        async move {
            #[derive(serde::Deserialize)]
            struct Resp { choices: Vec<Choice> }
            #[derive(serde::Deserialize)]
            struct Choice { message: Msg }
            #[derive(serde::Deserialize)]
            struct Msg { content: String }

            let body = serde_json::json!({
                "model": "local",
                "stream": false,
                "max_tokens": 512,
                "messages": [
                    {
                        "role": "system",
                        "content": format!(
                            "你是筆記摘要助手。以下是「{}」第 {} 段，請根據查詢提取相關要點，\
                            輸出 3-6 個條目，每點不超過 40 字。與查詢無關時回覆「本段無相關內容」。",
                            label, i + 1
                        )
                    },
                    {
                        "role": "user",
                        "content": format!("查詢：{}\n\n內容：\n{}", query, chunk)
                    }
                ]
            });
            let resp = client
                .post(format!("{}/v1/chat/completions", base_url))
                .json(&body)
                .timeout(Duration::from_secs(60))
                .send().await.ok()?;
            let parsed: Resp = resp.json().await.ok()?;
            parsed.choices.into_iter().next().map(|c| c.message.content)
        }
    }).collect();

    let summaries: Vec<String> = futures::future::join_all(futs)
        .await
        .into_iter()
        .flatten()
        .filter(|s| s.trim() != "本段無相關內容")
        .collect();

    if summaries.is_empty() {
        return None;
    }

    Some(format!(
        "[「{}」摘要，共 {} 段相關內容]\n{}",
        file_label,
        summaries.len(),
        summaries.join("\n---\n")
    ))
}

/// 封裝 OpenAI-compatible SSE 串流請求，返回 StreamResult
/// 同時處理文字 token（emit llm:token）和 tool call fragments 的累積
async fn send_streaming_request(
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
/// 前端選擇搜尋方式後呼叫（解除 call_external_ai 工具的暫停狀態）
#[tauri::command]
pub async fn confirm_search_method(
    state: State<'_, AppState>,
    method: String,
) -> Result<(), AppError> {
    if let Some(tx) = state.search_method_tx.lock().await.take() {
        let _ = tx.send(method);
    }
    Ok(())
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
    let vault_db = state.db.clone();

    // 3. 組裝 messages_json
    //    - conversation_id 存在：從 DB 載入歷史，追加當前 user 訊息
    //    - 否則：使用前端傳入的 messages（向下相容）
    let mut messages_json: Vec<serde_json::Value> = if let Some(ref conv_id) = conversation_id {
        let mut db_msgs = load_messages(&state.db, conv_id).await?;
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
        // 若第一條已是 system，覆蓋它；否則插入
        if messages_json.first().and_then(|m| m["role"].as_str()) == Some("system") {
            messages_json[0] = serde_json::json!({"role": "system", "content": sys});
        } else {
            messages_json.insert(0, serde_json::json!({"role": "system", "content": sys}));
        }
    }

    // 4. 建立 ToolRegistry（vault 可用時注入工具）
    // 使用 llama-server 處理所有 agent trigger embedding（chat 就緒時必然可用）
    let reg_emb_url: Option<String> = Some(base_url.clone());
    let skill_emb_url = reg_emb_url.clone(); // 保留一份給 skill pre-pass 使用
    let search_method_tx = Arc::clone(&state.search_method_tx);

    // 延遲繫結 handle（供 spawn_sub_agent 工具使用）
    let llm_fn_late = crate::tools::make_late_llm_fn();
    let registry_late: Arc<tokio::sync::Mutex<Option<Arc<ToolRegistry>>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    let registry = if !vault_path.is_empty() && vault_id_opt.is_some() {
        crate::tools::build_vault_registry(
            vault_path.clone(),
            vault_db.clone(),
            vault_id_opt.clone().unwrap_or_default(),
            app.clone(),
            reg_emb_url.clone(),
            search_method_tx,
            Arc::clone(&llm_fn_late),
            Arc::clone(&registry_late),
            Arc::clone(&state.system_agent),
            Some(Arc::clone(&state.agent_cancel)),
            Arc::clone(&state.api_key_cache),
            Arc::clone(&state.settings_cache),
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
        if let Ok(Some(pending)) = load_pending_plan(&state.db, conv_id).await {
            let age = chrono::Utc::now().timestamp() - pending.created_at;
            let _ = delete_pending_plan(&state.db, conv_id).await;
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

    // 10. 記憶事實語意搜尋（embedding 相似度優先，fallback 最新事實）
    // query_vec 提升到此 scope，供後續 skill pre-pass 共用，避免重複呼叫 /embedding
    let (memory_context, query_vec_opt) = if let Some(ref vid) = vault_id_opt {
        let query_vec = get_embedding(&client, &base_url, &input).await;
        let mem = retrieve_relevant_facts(&vault_db, vid, &input, &query_vec, 10).await;
        let vec_opt = if query_vec.is_empty() { None } else { Some(query_vec) };
        (mem, vec_opt)
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

        let tools = vault_tools();
        let dispatcher = Dispatcher::new(Arc::clone(&registry));
        let tx = Arc::new(Transaction::new());
        let _ = tx.prepare().await;
        let mut final_text = String::new();

        // Skill pre-pass：active skills（永遠注入）+ passive skills（embedding 相似度匹配）
        // 上限 1500 chars，保護 system message budget
        if let Some(ref vid) = vault_id_opt {
            if let Some((skill_text, _skill_titles)) = run_skill_pre_pass(
                &vault_db, vid, &input, skill_emb_url.as_deref(), &client, "main",
                query_vec_opt.clone(),  // 共用 memory 步驟已算好的向量
            ).await {
                if let Some(sys) = msgs.first_mut() {
                    if sys["role"].as_str() == Some("system") {
                        let existing = sys["content"].as_str().unwrap_or("").to_string();
                        // 技能文字上限 1500 chars（防止過多 active skills 撐爆 context）
                        let skill_snippet: String = skill_text.chars().take(1500).collect();
                        let new_content = format!("{}\n\n{}", existing, skill_snippet);
                        *sys = serde_json::json!({"role": "system", "content": new_content});
                    }
                }
            }
        }

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
                Err(e) => { eprintln!("[chat] llm error: {e}"); break; }
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
                // Tool result 上限 3000 chars，防止大型 read_note 繞過 sliding window
                const MAX_TOOL_RESULT: usize = 3000;
                let res_str = if raw.chars().count() > MAX_TOOL_RESULT {
                    // read_note：嘗試從 chunks 表並行摘要；其他工具截斷
                    if name == "read_note" {
                        if let (Some(vid), Some(fp)) = (vault_id_opt.as_deref(), args["path"].as_str()) {
                            if let Some(summary) = parallel_chunk_summarize(&vault_db, vid, fp, &input, &client, &base_url).await {
                                summary
                            } else {
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
        let _ = save_messages(&state.db, conv_id, &arr).await;

        // maybe_set_title：只有首次（標題尚未設定）才需呼叫；之後用 in-memory set 跳過
        let already_titled = state.titled_convs.lock().await.contains(conv_id.as_str());
        if !already_titled {
            let _ = maybe_set_title(&state.db, conv_id, &arr).await;
            state.titled_convs.lock().await.insert(conv_id.clone());
        }
    }

    Ok(response_text)
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
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String, // 累積的 JSON fragment 字串
}

/// send_streaming_request 的回傳結果
struct StreamResult {
    full_text: String,
    finish_reason: String,
    tool_call_chunks: Vec<ToolCallAccumulator>,
}

/// 讀取最近對話，回傳摘要供 reflection agent 分析模式
pub async fn tool_list_recent_conversations(db: &crate::db::surreal::SurrealDb, vault_id: &str, limit: usize) -> String {
    #[derive(serde::Deserialize)]
    struct ConvRow {
        title: Option<String>,
        mode: Option<String>,
        messages_json: Option<String>,
    }
    let limit = limit.min(20) as i64;
    let mut resp = match db.query(
        "SELECT title, mode, messages_json FROM conversations \
         WHERE (vault_id = $vid OR vault_id = NONE) AND messages_json != '[]' \
         ORDER BY updated_at DESC LIMIT $lim"
    ).bind(("vid", vault_id.to_string())).bind(("lim", limit)).await {
        Ok(r) => r,
        Err(e) => return format!("查詢失敗：{e}"),
    };
    let rows: Vec<ConvRow> = resp.take(0).unwrap_or_default();
    if rows.is_empty() { return "沒有找到任何對話記錄".to_string(); }

    let mut out = format!("最近 {} 段對話：\n\n", rows.len());
    for (i, row) in rows.iter().enumerate() {
        let title = row.title.as_deref().unwrap_or("未命名");
        let mode  = row.mode.as_deref().unwrap_or("chat");
        out.push_str(&format!("## 對話 {} — {} ({})\n", i + 1, title, mode));

        // 取最後 6 則 user/assistant 訊息
        if let Some(ref json) = row.messages_json {
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

/// 建立新技能規範（供 reflection agent 呼叫，預設未啟用）
pub async fn tool_create_agent_skill(
    db: &crate::db::surreal::SurrealDb,
    vault_id: &str,
    title: &str,
    trigger: &str,
    behavior: &str,
    injection_mode: &str,
    emb_url: Option<&str>,
) -> String {
    let skill_id = uuid::Uuid::new_v4().to_string();
    let mode = if injection_mode == "active" { "active" } else { "passive" };
    let client = reqwest::Client::new();
    let trigger_embedding: Option<Vec<f32>> = if let Some(url) = emb_url {
        let v = get_embedding(&client, url, trigger).await;
        if v.is_empty() { None } else { Some(v) }
    } else { None };

    let result = db.query(
        "INSERT INTO agent_skills \
         (skill_id, vault_id, knowledge_item_id, title, trigger, behavior, \
          auto_tool_calls, is_active, injection_mode, trigger_count, trigger_embedding, created_at) \
         VALUES ($sid, $vid, 'reflection', $title, $trigger, $behavior, \
                 [], false, $mode, 0, $emb, time::now())"
    )
    .bind(("sid", skill_id.clone()))
    .bind(("vid", vault_id.to_string()))
    .bind(("title", title.to_string()))
    .bind(("trigger", trigger.to_string()))
    .bind(("behavior", behavior.to_string()))
    .bind(("mode", mode.to_string()))
    .bind(("emb", trigger_embedding))
    .await;

    match result {
        Ok(_)  => format!("✅ 技能「{}」已建立（未啟用，請至技能規範頁面審核）", title),
        Err(e) => format!("❌ 建立失敗：{e}"),
    }
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
                "name": "call_external_ai",
                "description": "呼叫外部 AI 服務（如 OpenAI / Anthropic）獲取即時資訊或當前事件。\
僅在問題需要本地模型不具備的最新外部資料時使用（例如今日新聞、即時排行、最新活動等）。\
不用於查詢 Vault 筆記或歷史對話（那些請用 search_vault）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "發送給外部 AI 的完整問題或指令"
                        }
                    },
                    "required": ["query"]
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

/// 將 vault_tools() 清單種入 agent_tools 表（冪等）。
/// 有 emb_url 時同步計算 embedding；無則留 NULL，之後可補填。
pub async fn seed_agent_tools(db: &SurrealDb, emb_url: Option<&str>) {
    let tools_json = vault_tools();
    let client = reqwest::Client::new();

    let tools = match tools_json.as_array() {
        Some(v) => v.clone(),
        None => return,
    };

    for tool in &tools {
        let func = &tool["function"];
        let name = func["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() { continue; }
        let description = func["description"].as_str().unwrap_or("").to_string();
        let schema = serde_json::to_string(tool).unwrap_or_default();

        // embedding（有伺服器時計算，失敗或無伺服器留 None）
        let embedding: Option<Vec<f32>> = if let Some(url) = emb_url {
            let emb = get_embedding(&client, url, &description).await;
            if emb.is_empty() { None } else { Some(emb) }
        } else {
            None
        };

        // 用 name 作為固定 record ID（確保唯一且可以 UPSERT）
        let _ = db.query(
            "INSERT INTO agent_tools (tool_id, name, description, schema_json, is_active, is_builtin)
             VALUES ($name, $name, $description, $schema_json, true, true)
             ON DUPLICATE KEY UPDATE description = $description, schema_json = $schema_json;"
        )
        .bind(("name", name.clone()))
        .bind(("description", description.clone()))
        .bind(("schema_json", schema))
        .await;

        // 若有 embedding 且尚未設定，補填
        if let Some(ref emb) = embedding {
            let _ = db.query(
                "UPDATE agent_tools SET embedding = $emb WHERE name = $name AND embedding = NONE;"
            )
            .bind(("emb", emb.clone()))
            .bind(("name", name))
            .await;
        }
    }
}

/// 依查詢向量從 agent_tools 表找出最相關的 top_k 個工具。
/// - 有 embedding 時：余弦相似度排序
/// - 無 embedding 時：回傳全部 (fallback = vault_tools())
/// 永遠包含 plan_announce 工具（寫入確認機制必需）。
#[allow(dead_code)]
pub async fn find_relevant_tools_for_query(
    db: &SurrealDb,
    query: &str,
    emb_url: Option<&str>,
    top_k: usize,
) -> serde_json::Value {
    // 必須永遠包含的工具（功能關鍵，不能被過濾掉）
    const ALWAYS_INCLUDE: &[&str] = &["plan_announce"];

    // ── 嘗試 embedding 相似度搜尋 ─────────────────────────────────
    if let Some(url) = emb_url {
        let client = reqwest::Client::new();
        let query_emb = get_embedding(&client, url, query).await;
        if !query_emb.is_empty() {
            #[derive(serde::Deserialize)]
            struct ToolRow {
                name: String,
                schema_json: String,
                embedding: Option<Vec<f32>>,
            }

            let rows: Vec<ToolRow> = db
                .query("SELECT name, schema_json, embedding FROM agent_tools WHERE is_active = true")
                .await
                .and_then(|mut r| r.take(0))
                .unwrap_or_default();

            // 只對有 embedding 的 row 計算分數
            let mut scored: Vec<(f32, &ToolRow)> = rows
                .iter()
                .filter_map(|row| {
                    row.embedding.as_ref().map(|emb| {
                        (cosine_similarity(&query_emb, emb), row)
                    })
                })
                .collect();

            if !scored.is_empty() {
                scored.sort_by(|a, b| {
                    b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
                });

                let mut result: Vec<serde_json::Value> = Vec::new();
                let mut included: std::collections::HashSet<String> =
                    std::collections::HashSet::new();

                // 先加入必須工具
                for row in &rows {
                    if ALWAYS_INCLUDE.contains(&row.name.as_str()) {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&row.schema_json) {
                            result.push(v);
                            included.insert(row.name.clone());
                        }
                    }
                }

                // 再加入 top-K（略過已包含）
                let mut count = 0;
                for (_, row) in &scored {
                    if count >= top_k { break; }
                    if included.contains(&row.name) { continue; }
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&row.schema_json) {
                        result.push(v);
                        included.insert(row.name.clone());
                        count += 1;
                    }
                }

                if !result.is_empty() {
                    return serde_json::Value::Array(result);
                }
            }
        }
    }

    // ── Fallback：回傳全部工具 ─────────────────────────────────────
    vault_tools()
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

#[derive(Debug, Clone)]
enum Comparison {
    LessThan(f64),
    LessThanOrEqual(f64),
    GreaterThan(f64),
    GreaterThanOrEqual(f64),
    Equal(f64),
    About(f64), // ±15%
}

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

/// 全文搜索 Vault（使用 SurrealDB BM25 FTS），支援比較條件過濾
pub(crate) async fn tool_search_vault(query: &str, vault_db: &SurrealDb, vault_id: &str, app: &AppHandle) -> String {
    if query.trim().is_empty() {
        return "請提供搜索關鍵字".to_string();
    }

    // 1. 清洗 query
    let cleaned = clean_fts_query(query);
    let (cmp, search_query) = parse_comparison(&cleaned);
    let fts_query = {
        let q = if search_query.trim().is_empty() { cleaned.clone() } else { search_query.trim().to_string() };
        clean_fts_query(&q)
    };

    let _ = app.emit("llm:stderr", format!(
        "[search] query: {:?} → fts: {:?} cmp: {}",
        query, fts_query,
        cmp.as_ref().map(|c| c.label()).unwrap_or_else(|| "無".to_string())
    ));

    // 2. 嘗試 chunk-based 搜尋（有 chunks 時使用）
    #[derive(Deserialize)]
    struct CountRow { count: i64 }
    let mut cr = vault_db.query("SELECT count() AS count FROM chunks WHERE vault_id = $vid GROUP ALL")
        .bind(("vid", vault_id.to_owned()))
        .await.ok();
    let chunk_count = cr.as_mut()
        .and_then(|r| r.take::<Vec<CountRow>>(0).ok())
        .and_then(|rows| rows.first().map(|r| r.count))
        .unwrap_or(0);

    if chunk_count > 0 && cmp.is_none() {
        if let Ok(result) = search_chunks_with_graph(vault_db, vault_id, &fts_query).await {
            if !result.is_empty() {
                return result;
            }
        }
    }

    // 3. Fallback：notes 全文搜尋（比較條件查詢 / chunk 為空時）
    #[derive(Deserialize)]
    struct NoteRow { path: String, title: String, status: Option<String> }
    let mut resp = vault_db.query(
        "SELECT path, title, status FROM notes WHERE vault_id = $vid AND (title @1@ $q OR content @2@ $q) ORDER BY search::score(1) + search::score(2) DESC LIMIT 15"
    )
    .bind(("vid", vault_id.to_owned()))
    .bind(("q", fts_query.clone()))
    .await;

    match resp {
        Err(e) => format!("搜索失敗：{}", e),
        Ok(ref mut r) => {
            let rows: Vec<NoteRow> = r.take(0).unwrap_or_default();
            if rows.is_empty() {
                return format!("未找到包含「{}」的筆記", fts_query);
            }
            let mut result_lines = Vec::new();
            for row in &rows {
                let path = &row.path;
                let title = &row.title;
                let status_badge = match row.status.as_deref() {
                    Some("verified") => " ✓",
                    _ => "",
                };

                // Fetch content for snippet/comparison
                #[derive(Deserialize)]
                struct ContentRow { content: String }
                let mut cr2 = vault_db.query(
                    "SELECT content FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1"
                )
                .bind(("vid", vault_id.to_owned()))
                .bind(("path", path.clone()))
                .await.ok();
                let content: Option<String> = cr2.as_mut()
                    .and_then(|r| r.take::<Vec<ContentRow>>(0).ok())
                    .and_then(|rows| rows.into_iter().next().map(|r| r.content));

                let snippet = if let Some(ref c) = content {
                    if let Some(ref cmp_ref) = cmp {
                        let matched = filter_lines_by_comparison(c, cmp_ref);
                        if matched.is_empty() { continue; }
                        format!("（符合條件的行）\n{}", matched.join("\n"))
                    } else {
                        let q = fts_query.to_lowercase();
                        let cl = c.to_lowercase();
                        if let Some(pos) = cl.find(&q) {
                            let mut start = pos.saturating_sub(60);
                            while start > 0 && !c.is_char_boundary(start) { start -= 1; }
                            let mut end = (pos + q.len() + 100).min(c.len());
                            while end < c.len() && !c.is_char_boundary(end) { end += 1; }
                            format!("...{}...", c[start..end].trim())
                        } else {
                            c.chars().take(120).collect::<String>() + "..."
                        }
                    }
                } else { String::new() };

                result_lines.push(format!("- **{}{}** ({})\n  {}", title, status_badge, path, snippet));
            }
            if result_lines.is_empty() {
                format!("在「{}」相關筆記中，未找到數值{}的項目", fts_query,
                    cmp.as_ref().map(|c| c.label()).unwrap_or_default())
            } else {
                let header = if let Some(ref c) = cmp {
                    format!("搜索「{}」，篩選數值{}，找到 {} 筆：", fts_query, c.label(), result_lines.len())
                } else {
                    format!("找到 {} 篇相關筆記：", result_lines.len())
                };
                format!("{}\n{}", header, result_lines.join("\n"))
            }
        }
    }
}

/// Chunk-based search + 1-hop graph expansion
async fn search_chunks_with_graph(vault_db: &SurrealDb, vault_id: &str, fts_query: &str) -> Result<String, crate::error::AppError> {
    // ── Step 1: BM25 FTS on chunks ─────────────────────────────────────
    #[derive(Deserialize)]
    struct ChunkRow { file_path: String, section: String, content: String }
    let mut resp = vault_db.query(
        "SELECT file_path, section, content FROM chunks WHERE vault_id = $vid AND content @1@ $q ORDER BY search::score(1) DESC LIMIT 10"
    )
    .bind(("vid", vault_id.to_owned()))
    .bind(("q", fts_query.to_owned()))
    .await
    .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    let chunk_rows: Vec<ChunkRow> = resp.take(0).map_err(|e| crate::error::AppError::Database(e.to_string()))?;

    if chunk_rows.is_empty() {
        return Ok(String::new());
    }

    // ── Step 2: Collect matched file paths ─────────────────────────────
    let matched_paths: Vec<String> = chunk_rows
        .iter()
        .map(|r| r.file_path.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    // ── Step 3: Graph expansion — find 1-hop linked files ───────────────
    let mut expanded_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in &matched_paths {
        // Outgoing links
        #[derive(Deserialize)] struct TargetRow { target_path: Option<String> }
        let mut out_resp = vault_db.query(
            "SELECT target_path FROM links WHERE vault_id = $vid AND source_path = $path AND target_path != NONE AND link_type = 'wikilink'"
        )
        .bind(("vid", vault_id.to_owned()))
        .bind(("path", path.clone()))
        .await.unwrap_or_else(|_| unreachable!());
        let out_rows: Vec<TargetRow> = out_resp.take(0).unwrap_or_default();

        // Incoming links
        #[derive(Deserialize)] struct SourceRow { source_path: String }
        let mut inc_resp = vault_db.query(
            "SELECT source_path FROM links WHERE vault_id = $vid AND target_path = $path AND link_type = 'wikilink'"
        )
        .bind(("vid", vault_id.to_owned()))
        .bind(("path", path.clone()))
        .await.unwrap_or_else(|_| unreachable!());
        let inc_rows: Vec<SourceRow> = inc_resp.take(0).unwrap_or_default();

        for r in out_rows {
            if let Some(p) = r.target_path {
                if !matched_paths.contains(&p) { expanded_paths.insert(p); }
            }
        }
        for r in inc_rows {
            if !matched_paths.contains(&r.source_path) { expanded_paths.insert(r.source_path); }
        }
    }

    // ── Step 4: Fetch titles for all relevant files ─────────────────────
    let all_paths: Vec<String> = matched_paths.iter().cloned()
        .chain(expanded_paths.iter().cloned()).collect();

    let mut titles: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut statuses: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for path in &all_paths {
        #[derive(Deserialize)] struct TitleRow { title: String, status: Option<String> }
        let mut tr = vault_db.query(
            "SELECT title, status FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1"
        )
        .bind(("vid", vault_id.to_owned()))
        .bind(("path", path.clone()))
        .await.ok();
        if let Some(t) = tr.as_mut()
            .and_then(|r| r.take::<Vec<TitleRow>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
        {
            titles.insert(path.clone(), t.title);
            if let Some(s) = t.status { statuses.insert(path.clone(), s); }
        }
    }

    // ── Step 5: Build result ────────────────────────────────────────────
    let mut result_lines = Vec::new();

    for row in &chunk_rows {
        let path = &row.file_path;
        let section = &row.section;
        let content = &row.content;
        let title = titles.get(path).cloned().unwrap_or_else(|| path.clone());
        let status_badge = match statuses.get(path).map(|s| s.as_str()) {
            Some("verified") => " ✓",
            _ => "",
        };

        let snippet: String = content.chars().take(200).collect();
        let section_label = if section.is_empty() { String::new() } else { format!(" § {}", section) };
        result_lines.push(format!("- **{}{}{}** ({})\n  {}…", title, status_badge, section_label, path, snippet.trim()));
    }

    if !expanded_paths.is_empty() {
        result_lines.push(String::from("\n📎 相關連結筆記（透過 wikilink 擴展）："));
        for path in &expanded_paths {
            let title = titles.get(path).cloned().unwrap_or_else(|| path.clone());
            let status_badge = match statuses.get(path).map(|s| s.as_str()) {
                Some("verified") => " ✓",
                _ => "",
            };
            result_lines.push(format!("- **{}{}** ({})", title, status_badge, path));
        }
    }

    Ok(format!("找到 {} 個相關段落：\n{}", chunk_rows.len(), result_lines.join("\n")))
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
    db_ctx: Option<(SurrealDb, String)>,
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
    // 同步到 notes table
    if let Some((db, vault_id)) = db_ctx {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let now_dt = surrealdb::sql::Datetime::from(
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms).unwrap_or_default()
        );
        let title = {
            let first = final_content.lines().find(|l| !l.trim().is_empty() && !l.starts_with("---") && !l.contains(':'))
                .unwrap_or(rel_path);
            first.trim_start_matches('#').trim().to_string()
        };
        let wc = final_content.split_whitespace().count() as i64;
        let checksum = format!("{:x}", sha2::Sha256::digest(final_content.as_bytes()));
        let _ = db.query(
            "INSERT INTO notes (vault_id, path, title, content, status, word_count, created_at, modified_at, checksum) \
             VALUES ($vid, $path, $title, $content, 'draft', $wc, $now, $now, $cs) \
             ON DUPLICATE KEY UPDATE title = $title, content = $content, status = 'draft', word_count = $wc, modified_at = $now, checksum = $cs"
        )
        .bind(("vid", vault_id.clone()))
        .bind(("path", rel_path.to_owned()))
        .bind(("title", title))
        .bind(("content", final_content.clone()))
        .bind(("wc", wc))
        .bind(("now", now_dt))
        .bind(("cs", checksum))
        .await;
    }
    format!("✅ 已建立筆記：{}", rel_path)
}

/// 更新現有筆記（覆寫全文）
pub(crate) async fn tool_update_note(
    rel_path: &str,
    content: &str,
    vault_path: &str,
    db_ctx: Option<(SurrealDb, String)>,
) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let final_content = inject_ai_frontmatter(content);
    if let Err(e) = tokio::fs::write(&abs_path, &final_content).await {
        return format!("更新失敗：{}", e);
    }
    // 同步到 notes table（AI 修改過的筆記重置為 draft，需重新驗證）
    if let Some((db, vault_id)) = db_ctx {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let now_dt = surrealdb::sql::Datetime::from(
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms).unwrap_or_default()
        );
        let title = {
            let first = final_content.lines().find(|l| !l.trim().is_empty() && !l.starts_with("---") && !l.contains(':'))
                .unwrap_or(rel_path);
            first.trim_start_matches('#').trim().to_string()
        };
        let wc = final_content.split_whitespace().count() as i64;
        let checksum = format!("{:x}", sha2::Sha256::digest(final_content.as_bytes()));
        let _ = db.query(
            "UPDATE notes SET content = $content, title = $title, status = 'draft', word_count = $wc, modified_at = $now, checksum = $cs \
             WHERE vault_id = $vid AND path = $path"
        )
        .bind(("content", final_content.clone()))
        .bind(("title", title))
        .bind(("wc", wc))
        .bind(("now", now_dt))
        .bind(("cs", checksum))
        .bind(("vid", vault_id))
        .bind(("path", rel_path.to_owned()))
        .await;
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

    let new_content: String = if on_disk {
        let abs_path = abs.as_ref().unwrap();
        let content = tokio::fs::read_to_string(abs_path).await
            .map_err(|e| AppError::AI(format!("Read failed: {}", e)))?;
        let updated = set_frontmatter_key(&content, "status", &status);
        tokio::fs::write(abs_path, &updated).await
            .map_err(|e| AppError::AI(format!("Write failed: {}", e)))?;
        // Sync to notes table
        let _ = state.db.query(
            "UPDATE notes SET content = $content WHERE vault_id = $vid AND path = $path"
        )
        .bind(("content", updated.clone()))
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await;
        updated
    } else {
        // KB import page — read/write content_md in import_pages
        #[derive(serde::Deserialize)]
        struct ContentRow { content_md: Option<String> }
        let mut resp = state.db.query(
            "SELECT content_md FROM import_pages WHERE vault_id = $vid AND note_path = $path LIMIT 1"
        )
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await
        .map_err(|e| AppError::AI(format!("DB read failed: {}", e)))?;
        let rows: Vec<ContentRow> = resp.take(0).unwrap_or_default();
        let content = rows.into_iter().next()
            .and_then(|r| r.content_md)
            .unwrap_or_else(|| format!("---\nstatus: {}\n---\n\n", status));
        let updated = set_frontmatter_key(&content, "status", &status);
        // Write back to import_pages
        let _ = state.db.query(
            "UPDATE import_pages SET content_md = $content WHERE vault_id = $vid AND note_path = $path"
        )
        .bind(("content", updated.clone()))
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await;
        updated
    };

    // Re-upsert chunks（確保 chunks 存在，並帶正確 status）
    {
        let now_ms = chrono::Local::now().timestamp_millis();
        let chunks = crate::vault::chunker::chunk_note(&path, &new_content, now_ms);
        let emb_url: Option<String> = {
            let port = *state.embedding_actual_port.lock().await;
            port.map(|p| format!("http://127.0.0.1:{}", p))
        };
        let _ = crate::vault::chunker::upsert_chunks(&state.db, &vault_id, &chunks, emb_url.as_deref()).await;
    }
    // 若設為 verified，記錄 reviewed_at 時間戳
    if status == "verified" {
        let now_ms = chrono::Local::now().timestamp_millis();
        let _ = state.db.query(
            "UPDATE chunks SET reviewed_at = $ts WHERE vault_id = $vid AND file_path = $fp"
        )
        .bind(("ts", now_ms))
        .bind(("vid", vault_id))
        .bind(("fp", path))
        .await;
    }
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
fn detect_tool_calls(
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
    vault_db: Option<(&SurrealDb, &str)>,
    vault_path: &str,
    app: &AppHandle,
    ext_config: &ExtAiConfig,
) -> String {
    // call_external_ai 不依賴 vault，可在 vault 未設定時使用
    if name == "call_external_ai" {
        let query = args["query"].as_str().unwrap_or("");
        return call_external_ai_tool(query, ext_config, app).await;
    }

    if vault_path.is_empty() {
        return "Vault 未設定，無法執行 Vault 操作".to_string();
    }
    match name {
        "search_vault" => {
            let query = args["query"].as_str().unwrap_or("");
            match vault_db {
                Some((db, vid)) => tool_search_vault(query, db, vid, app).await,
                None => "Vault 資料庫未就緒".to_string(),
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
            let db_ctx = vault_db.map(|(db, vid)| (db.clone(), vid.to_owned()));
            tool_create_note(&path, content, vault_path, db_ctx).await
        }
        "update_note" => {
            let path = ensure_md(args["path"].as_str().unwrap_or(""));
            let content = args["content"].as_str().unwrap_or("");
            let db_ctx = vault_db.map(|(db, vid)| (db.clone(), vid.to_owned()));
            tool_update_note(&path, content, vault_path, db_ctx).await
        }
        "create_folder" => {
            let path = args["path"].as_str().unwrap_or("");
            tool_create_folder(path, vault_path).await
        }
        "query_memory" => {
            let keywords: Vec<String> = args["keywords"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let since = args["since"].as_str().map(String::from);
            let limit = args["limit"].as_u64().map(|v| v as usize);
            match vault_db {
                Some((db, vid)) => tool_query_memory(keywords, since, limit, db, vid, None).await,
                None => "Vault 資料庫未就緒".to_string(),
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
    let vault_id_opt = state.get_vault_id().await.ok();
    let vault_db = state.db.clone();

    let all_tool_names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();

    // ── Emit prepare ─────────────────────────────────────────────────────────
    let _ = app.emit("agent:tx_debug", serde_json::json!({
        "session_id": session_id,
        "kind": "prepare",
        "tools": all_tool_names,
    }));

    // 只有 call_external_ai 需要 ext_config，避免不必要的 keychain 存取
    let needs_ext = steps.iter().any(|s| s.name == "call_external_ai");
    let ext_config = if needs_ext {
        let db = &state.db;
        let ext_provider = get_cached_setting(&state.settings_cache, db, "ai_provider", "").await;
        let ext_base_url = get_cached_setting(&state.settings_cache, db, "ai_base_url", "https://api.openai.com/v1").await;
        let ext_model = get_cached_setting(&state.settings_cache, db, "ai_model", "gpt-4o").await;
        let ext_api_key = read_api_key(&state.api_key_cache, &ext_provider).await;
        ExtAiConfig { provider: ext_provider, base_url: ext_base_url, model: ext_model, api_key: ext_api_key }
    } else {
        ExtAiConfig { provider: String::new(), base_url: String::new(), model: String::new(), api_key: String::new() }
    };

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

        let vault_db_ref = vault_id_opt.as_deref().map(|vid| (&vault_db, vid));
        let start = std::time::Instant::now();
        let output = execute_vault_tool(
            &step.name,
            &step.args,
            vault_db_ref,
            &vault_path,
            &app,
            &ext_config,
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
    let db = state.db.clone();
    let vault_id_opt = state.get_vault_id().await.ok();

    // ── Emit prepare ─────────────────────────────────────────────────────────
    let _ = app.emit("agent:tx_debug", serde_json::json!({
        "session_id": session_id,
        "kind": "prepare",
        "tools": [tool_name.clone()],
    }));

    // Only build ext_config for call_external_ai — other tools ignore it entirely.
    let ext_config = if tool_name == "call_external_ai" {
        let ext_provider = get_cached_setting(&state.settings_cache, &db, "ai_provider", "").await;
        let ext_base_url = get_cached_setting(&state.settings_cache, &db, "ai_base_url", "https://api.openai.com/v1").await;
        let ext_model = get_cached_setting(&state.settings_cache, &db, "ai_model", "gpt-4o").await;
        let ext_api_key = read_api_key(&state.api_key_cache, &ext_provider).await;
        ExtAiConfig { provider: ext_provider, base_url: ext_base_url, model: ext_model, api_key: ext_api_key }
    } else {
        ExtAiConfig { provider: String::new(), base_url: String::new(), model: String::new(), api_key: String::new() }
    };

    let result = execute_vault_tool(
        &tool_name,
        &args,
        vault_id_opt.as_deref().map(|vid| (&db, vid)),
        &vault_path,
        &app,
        &ext_config,
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

// ── Agent Skill Pre-pass ──────────────────────────────────────────────────────

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

/// Skill 記錄（輕量版，只含 pre-pass 所需欄位）
#[derive(Deserialize)]
struct ActiveSkillRow {
    skill_id: String,
    title: String,
    trigger: String,
    behavior: String,
    auto_tool_calls: Vec<String>,
    #[allow(dead_code)]
    #[serde(default = "passive_str")]
    injection_mode: String,
    #[serde(default = "scope_all_str")]
    agent_scope: String,
}

fn passive_str() -> String { "passive".to_string() }
fn scope_all_str() -> String { "all".to_string() }

/// 取得所有 injection_mode = 'active' 的啟用技能（永遠注入，不做 embedding 比對）。
/// `caller_scope` 為 "main" / "search" / "write" / "research" / "memory"；
/// 只回傳 agent_scope = 'all' 或 agent_scope = caller_scope 的技能。
async fn fetch_always_on_skills(
    db: &surrealdb::Surreal<surrealdb::engine::local::Db>,
    vault_id: &str,
    caller_scope: &str,
) -> Vec<ActiveSkillRow> {
    let mut resp = db.query(
        "SELECT skill_id, title, trigger, behavior, auto_tool_calls, injection_mode, \
                agent_scope OR 'all' AS agent_scope \
         FROM agent_skills \
         WHERE vault_id = $vid AND is_active = true AND injection_mode = 'active' \
           AND (agent_scope = 'all' OR agent_scope = NONE OR agent_scope = $scope) \
         ORDER BY created_at ASC"
    )
    .bind(("vid", vault_id.to_string()))
    .bind(("scope", caller_scope.to_string()))
    .await.ok();
    resp.as_mut()
        .and_then(|r| r.take::<Vec<ActiveSkillRow>>(0).ok())
        .unwrap_or_default()
}

/// 向量搜尋相似度 > SKILL_THRESHOLD 的 passive skills；
/// 若 trigger_embedding 為 None，fallback 到文字 contains 匹配。
async fn search_passive_skills(
    db: &surrealdb::Surreal<surrealdb::engine::local::Db>,
    vault_id: &str,
    query_embedding: &[f32],
    caller_scope: &str,
) -> Vec<ActiveSkillRow> {
    const SKILL_THRESHOLD: f64 = 0.65;

    // 向量搜尋（有 embedding 的 passive skills）
    let mut vector_results: Vec<ActiveSkillRow> = {
        let emb_val: Vec<f64> = query_embedding.iter().map(|&x| x as f64).collect();
        let mut resp = db.query(
            "SELECT skill_id, title, trigger, behavior, auto_tool_calls, injection_mode, \
                    agent_scope OR 'all' AS agent_scope \
             FROM agent_skills \
             WHERE vault_id = $vid AND is_active = true AND injection_mode != 'active' \
               AND (agent_scope = 'all' OR agent_scope = NONE OR agent_scope = $scope) \
               AND trigger_embedding != NONE \
               AND vector::similarity::cosine(trigger_embedding, $qvec) > $thresh \
             ORDER BY vector::similarity::cosine(trigger_embedding, $qvec) DESC \
             LIMIT 4"
        )
        .bind(("vid", vault_id.to_string()))
        .bind(("scope", caller_scope.to_string()))
        .bind(("qvec", emb_val))
        .bind(("thresh", SKILL_THRESHOLD))
        .await.ok();
        resp.as_mut()
            .and_then(|r| r.take::<Vec<ActiveSkillRow>>(0).ok())
            .unwrap_or_default()
    };

    // Fallback：text contains 匹配（trigger_embedding 為 None 的 passive skills）
    let mut text_results: Vec<ActiveSkillRow> = {
        let mut resp = db.query(
            "SELECT skill_id, title, trigger, behavior, auto_tool_calls, injection_mode, \
                    agent_scope OR 'all' AS agent_scope \
             FROM agent_skills \
             WHERE vault_id = $vid AND is_active = true AND injection_mode != 'active' \
               AND (agent_scope = 'all' OR agent_scope = NONE OR agent_scope = $scope) \
               AND trigger_embedding = NONE \
             LIMIT 10"
        )
        .bind(("vid", vault_id.to_string()))
        .bind(("scope", caller_scope.to_string()))
        .await.ok();
        resp.as_mut()
            .and_then(|r| r.take::<Vec<ActiveSkillRow>>(0).ok())
            .unwrap_or_default()
    };

    vector_results.append(&mut text_results);
    vector_results.truncate(4);
    vector_results
}

/// 將 matched skills 格式化為「# 使用者技能規範」system prompt 區塊。
fn build_skill_injection_section(skills: &[ActiveSkillRow]) -> String {
    let mut section = String::from(
        "# 使用者技能規範\n\
         以下規範由使用者從個人知識庫設定，本次對話自動啟用，請優先遵守：\n\n"
    );
    for skill in skills {
        section.push_str(&format!(
            "## {}\n**觸發條件**：{}\n**行為規範**：{}\n\n",
            skill.title, skill.trigger, skill.behavior
        ));
    }
    section
}

/// 更新 trigger_count + last_triggered_at，並寫入 skill_usage_log（供趨勢圖）。
async fn bump_skill_trigger_count(
    db: &surrealdb::Surreal<surrealdb::engine::local::Db>,
    vault_id: &str,
    skill_ids: &[String],
) {
    if skill_ids.is_empty() { return; }
    let _ = db.query(
        "UPDATE agent_skills SET trigger_count += 1, last_triggered_at = time::now() \
         WHERE vault_id = $vid AND skill_id IN $ids"
    )
    .bind(("vid", vault_id.to_string()))
    .bind(("ids", skill_ids.to_vec()))
    .await;

    // 寫入使用記錄
    for sid in skill_ids {
        let log_id = uuid::Uuid::new_v4().to_string();
        let _ = db.query(
            "INSERT INTO skill_usage_log (log_id, vault_id, skill_id, triggered_at) \
             VALUES ($lid, $vid, $sid, time::now())"
        )
        .bind(("lid", log_id))
        .bind(("vid", vault_id.to_string()))
        .bind(("sid", sid.clone()))
        .await;
    }
}

/// Pre-pass 入口：
/// 1. 主動注入（injection_mode = 'active'）：永遠注入，不做 embedding 比對。
/// 2. 被動取用（injection_mode = 'passive'）：embed query → cosine 相似度 > 閾值才注入。
/// 回傳 Some((injection_text, titles)) 若有任何符合的 skills，否則 None。
async fn run_skill_pre_pass(
    db: &surrealdb::Surreal<surrealdb::engine::local::Db>,
    vault_id: &str,
    query: &str,
    emb_url: Option<&str>,
    client: &reqwest::Client,
    caller_scope: &str,
    precomputed_embedding: Option<Vec<f32>>,  // 傳入已算好的向量，避免重複呼叫 /embedding
) -> Option<(String, Vec<String>)> {
    // 1. 主動注入 skills（不需要 embedding server）
    let always_on = fetch_always_on_skills(db, vault_id, caller_scope).await;

    // 2. 被動取用 skills（需要 embedding server；優先使用外部傳入的向量）
    let passive = if let Some(url) = emb_url {
        let query_vec = match precomputed_embedding {
            Some(v) if !v.is_empty() => v,
            _ => get_embedding(client, url, query).await,
        };
        if query_vec.is_empty() {
            vec![]
        } else {
            search_passive_skills(db, vault_id, &query_vec, caller_scope).await
        }
    } else {
        vec![]
    };

    // 合併，去重（active 優先）
    let mut seen = std::collections::HashSet::new();
    let mut matched: Vec<ActiveSkillRow> = Vec::new();
    for s in always_on.into_iter().chain(passive.into_iter()) {
        if seen.insert(s.skill_id.clone()) {
            matched.push(s);
        }
    }

    if matched.is_empty() { return None; }

    // 更新觸發統計
    let ids: Vec<String> = matched.iter().map(|s| s.skill_id.clone()).collect();
    bump_skill_trigger_count(db, vault_id, &ids).await;

    let titles: Vec<String> = matched.iter().map(|s| s.title.clone()).collect();

    let auto_tools: Vec<String> = matched.iter()
        .flat_map(|s| s.auto_tool_calls.iter().cloned())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let mut section = build_skill_injection_section(&matched);

    if !auto_tools.is_empty() {
        section.push_str(&format!(
            "**自動工具提示**：請在第一輪回答前先呼叫以下工具以獲取相關知識：{}\n\n",
            auto_tools.join("、")
        ));
    }

    Some((section, titles))
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
