// agent.rs
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::dispatcher::Dispatcher;
use super::intent_classifier::{Intent, IntentClassifier};
use super::memory_agent::MemoryAgent;
use super::planner::Planner;
use super::transaction::Transaction;
use super::types::{ConfirmWriteFn, EmbedFn, EmitEventFn, LlmFn, PrefetchFn, TxDebugEvent};

pub struct Agent {
    dispatcher: Dispatcher,
    intent_classifier: IntentClassifier,
    /// LLM 串流回呼（由外層注入，避免 runtime 直接依賴 reqwest/tauri）
    llm_fn: LlmFn,
    /// 寫入工具確認回呼
    confirm_write: ConfirmWriteFn,
    /// 通用事件 emit 回呼
    emit: EmitEventFn,
    /// Vault 根目錄路徑（用於 note_refs 絕對路徑組裝）
    vault_path: String,
    /// LLM HTTP 串流取消旗標（與 AppState.agent_cancel 共享同一個 Arc）
    stream_cancel: Arc<AtomicBool>,
    /// 目前活躍的 session_id（與 AppState.agent_session 共享同一個 Arc）
    current_session: Arc<Mutex<Option<String>>>,
    /// 預設工具列表（ToolUse/Chat 路徑使用；None = 不傳工具給 LLM）
    vault_tools: Option<Value>,
    /// 記憶預取回呼（Intent::Memory 路徑的初始種子；None = 無 vault DB）
    prefetch_memory: Option<PrefetchFn>,
    /// Embedding 回呼（用於 plan_announce centroid 計算）
    embed_fn: EmbedFn,
    /// settings DB（用於 pending_plans CRUD）
    settings_db: SqlitePool,
    /// 目前對話的 conversation_id（None = 無 DB 模式）
    conversation_id: Option<String>,
}

impl Agent {
    pub fn new(
        dispatcher: Dispatcher,
        intent_classifier: IntentClassifier,
        llm_fn: LlmFn,
        confirm_write: ConfirmWriteFn,
        emit: EmitEventFn,
        vault_path: String,
        stream_cancel: Arc<AtomicBool>,
        current_session: Arc<Mutex<Option<String>>>,
        vault_tools: Option<Value>,
        prefetch_memory: Option<PrefetchFn>,
        embed_fn: EmbedFn,
        settings_db: SqlitePool,
        conversation_id: Option<String>,
    ) -> Self {
        Self {
            dispatcher,
            intent_classifier,
            llm_fn,
            confirm_write,
            emit,
            vault_path,
            stream_cancel,
            current_session,
            vault_tools,
            prefetch_memory,
            embed_fn,
            settings_db,
            conversation_id,
        }
    }

    /// 主執行函數
    ///
    /// - `user_input`：當前使用者輸入（用於意圖分類）
    /// - `messages`：已包含 system + 歷史 + 當前 user 訊息的完整陣列
    /// - `use_tools`：是否啟用工具（false → 純對話，不傳 tools 給 LLM）
    pub async fn run(
        &self,
        user_input: String,
        mut messages: Vec<Value>,
        use_tools: bool,
    ) -> Result<String, String> {
        use crate::commands::conversation::{
            load_pending_plan, delete_pending_plan,
        };

        // ── 檢查是否有 pending plan（conversation_id 模式）─────────────
        if let Some(ref conv_id) = self.conversation_id {
            let plan = load_pending_plan(&self.settings_db, conv_id).await
                .unwrap_or(None);

            if let Some(pending) = plan {
                // TTL 檢查：超過 24h 自動取消
                let age = chrono::Utc::now().timestamp() - pending.created_at;
                if age > 86400 {
                    let _ = delete_pending_plan(&self.settings_db, conv_id).await;
                    // 通知 LLM 計畫已過期，繼續正常處理
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": "[系統] 先前的操作計畫已過期（超過 24 小時）自動取消。"
                    }));
                } else {
                    // 用 embedding 分類 intent
                    let intent = self.intent_classifier.classify_with_embedding(
                        &user_input,
                        &pending.confirm_centroid,
                        &pending.cancel_centroid,
                        &pending.interrupt_centroid,
                        &self.embed_fn,
                    ).await;

                    // 無論任何 intent 都清除 pending plan
                    let _ = delete_pending_plan(&self.settings_db, conv_id).await;

                    match intent {
                        Intent::Confirm => {
                            // note-open plan：emit agent:open_note + 確認文字，不啟動 LLM
                            let is_note_open = pending.deferred_tools.first()
                                .map(|t| t.name == "__open_note__")
                                .unwrap_or(false);

                            if is_note_open {
                                let paths: Vec<Value> = pending.deferred_tools.iter()
                                    .flat_map(|t| {
                                        t.args["paths"].as_array()
                                            .cloned()
                                            .unwrap_or_default()
                                    })
                                    .collect();
                                let note_name = paths.first()
                                    .and_then(|p| p.as_str())
                                    .and_then(|p| p.split('/').last())
                                    .map(|n| n.trim_end_matches(".md").to_string())
                                    .unwrap_or_else(|| "筆記".to_string());
                                let confirm_text = format!("好的，已為你打開《{}》。", note_name);
                                (self.emit)("agent:open_note".into(), Value::Array(paths));
                                (self.emit)("llm:token".into(), Value::String(confirm_text.clone()));
                                (self.emit)("llm:done".into(), Value::String(confirm_text.clone()));
                                return Ok(confirm_text);
                            }

                            // 一般寫入計畫確認：重新走 streaming loop
                            let deferred_desc = pending.deferred_tools.iter()
                                .map(|t| format!("- {} {:?}", t.name, t.args))
                                .collect::<Vec<_>>().join("\n");
                            messages.push(serde_json::json!({
                                "role": "user",
                                "content": format!(
                                    "[系統] 使用者已確認，請立即執行以下計畫中的工具：\n{}",
                                    deferred_desc
                                )
                            }));
                            self.stream_cancel.store(false, Ordering::Relaxed);
                            let session_id = Uuid::new_v4().to_string();
                            *self.current_session.lock().await = Some(session_id.clone());
                            let tools = if use_tools { self.vault_tools.clone() } else { None };
                            return self.run_streaming_loop(messages, tools, session_id).await;
                        }
                        Intent::Cancel | Intent::Interrupt => {
                            (self.emit)("agent:cancelled".into(), Value::Null);
                            (self.emit)("llm:done".into(), Value::String(String::new()));
                            return Ok(String::new());
                        }
                        _ => {
                            // 無法辨識 → 自動 cancel，繼續當新查詢處理
                            messages.push(serde_json::json!({
                                "role": "user",
                                "content": "[系統] 先前的操作計畫已自動取消，因為偵測到新的查詢請求。"
                            }));
                            // 落穿至下方正常 intent classify
                        }
                    }
                }
            }
        }

        // ── 記憶預取（所有意圖共用，單次 DB query）────────────────────────
        // 結果暫存於 prefetched；Cancel/Confirm 路徑不會用到，但代價極低（<1ms 空查詢）
        let prefetched = if let Some(ref pf) = self.prefetch_memory {
            pf(user_input.clone()).await
        } else {
            String::new()
        };

        // 對 Chat/ToolUse 意圖：將記憶上下文追加到現有 system prompt 尾端
        // （Memory 意圖會在下方整個替換 messages[0]，此處注入會被覆蓋，故暫不插入）
        let intent = self.intent_classifier.classify(&user_input).await;

        match intent {
            // ── 取消 / 中斷 ───────────────────────────────────────────
            Intent::Cancel | Intent::Interrupt => {
                self.stream_cancel.store(true, Ordering::Relaxed);
                (self.emit)("agent:cancelled".into(), Value::Null);
                // 必須 emit llm:done，否則前端 sendToLLM 的 unlistenDone 永遠不觸發 → UI 卡在 thinking
                (self.emit)("llm:done".into(), Value::String(String::new()));
                return Ok(String::new());
            }

            // ── 確認（行內確認由 confirm_write 閉包處理；此處略過）────
            Intent::Confirm => {
                // 必須 emit llm:done，否則前端 sendToLLM 的 unlistenDone 永遠不觸發 → UI 卡在 thinking
                (self.emit)("llm:done".into(), Value::String(String::new()));
                return Ok(String::new());
            }

            // ── 記憶查詢 → 替換 system prompt + 限縮工具，走串流 loop ─
            Intent::Memory => {
                // 串聯優化：使用頂端已預取的記憶作為初始種子注入 MemoryAgent system prompt
                // LLM 可直接利用此上下文回答，或再呼叫 query_memory 工具深化搜尋
                let base_system = MemoryAgent::build_system_prompt();
                let memory_system = if prefetched.is_empty() {
                    base_system
                } else {
                    format!(
                        "{}\n\n【預先擷取的記憶上下文（可直接使用，也可再呼叫 query_memory 深化搜尋）】\n{}",
                        base_system, prefetched
                    )
                };

                if messages.first().and_then(|m| m["role"].as_str()) == Some("system") {
                    messages[0] = serde_json::json!({"role": "system", "content": memory_system});
                } else {
                    messages.insert(0, serde_json::json!({"role": "system", "content": memory_system}));
                }
                self.stream_cancel.store(false, Ordering::Relaxed);
                let session_id = Uuid::new_v4().to_string();
                *self.current_session.lock().await = Some(session_id.clone());
                self.run_streaming_loop(messages, Some(MemoryAgent::tools_definition()), session_id).await
            }

            // ── 工具使用 / 對話 → 多輪串流 LLM loop ─────────────────
            Intent::ToolUse | Intent::Chat => {
                // 將預取的記憶上下文追加到現有 system prompt 尾端
                if !prefetched.is_empty() {
                    let section = format!("\n\n以下是相關的過去對話記憶（供參考）：\n{}", prefetched);
                    if let Some(sys) = messages.first_mut().filter(|m| m["role"] == "system") {
                        let old = sys["content"].as_str().unwrap_or("").to_string();
                        sys["content"] = Value::String(old + &section);
                    }
                }
                self.stream_cancel.store(false, Ordering::Relaxed);
                let session_id = Uuid::new_v4().to_string();
                *self.current_session.lock().await = Some(session_id.clone());
                let tools = if use_tools { self.vault_tools.clone() } else { None };
                self.run_streaming_loop(messages, tools, session_id).await
            }
        }
    }

    /// 多輪 LLM 串流 loop（最多 5 輪工具呼叫）
    ///
    /// - `tools`：None = 不傳工具；Some(json) = 傳指定工具列表
    /// - 每輪 LLM 回應的所有工具呼叫由 Planner 轉成 ToolGraph，Dispatcher 執行
    async fn run_streaming_loop(
        &self,
        mut messages: Vec<Value>,
        tools: Option<Value>,
        session_id: String,
    ) -> Result<String, String> {
        use crate::commands::conversation::{save_pending_plan, DeferredTool};
        use crate::commands::ai::compute_centroid;

        // 建立 Transaction → prepare → emit
        let tx = Arc::new(Transaction::new());
        tx.prepare().await?;
        self.emit_tx(&session_id, "prepare", &tx).await;

        let cancel = Arc::clone(&self.stream_cancel);
        let mut final_text = String::new();
        // 跨輪次收集已提交的寫入工具（name, args），用於 commit 後 emit vault:changed
        let mut committed_writes: Vec<(String, Value)> = Vec::new();
        // plan_announce retry 計數器（最多反問 1 次）
        let mut plan_announce_retried = false;
        // 跨輪次收集所有 note refs（供 note-open pending plan 使用）
        let mut all_note_refs: Vec<String> = Vec::new();
        // plan_announce 是否已被呼叫（防止 note-open pending plan 覆蓋寫入 pending plan）
        let mut plan_announced = false;

        'outer: for _round in 0..5 {
            // 每輪前檢查取消旗標
            if cancel.load(Ordering::Relaxed) {
                let _ = tx.cancel().await;
                self.emit_tx(&session_id, "cancel", &tx).await;
                self.clear_session().await;
                return Ok(String::new());
            }

            // LLM 串流（token 由 llm_fn 內部 emit llm:token）
            let round = (self.llm_fn)(
                messages.clone(),
                tools.clone(),
                Some(Arc::clone(&cancel)),
            )
            .await?;

            // 串流完成後再次檢查
            if cancel.load(Ordering::Relaxed) {
                let _ = tx.cancel().await;
                self.emit_tx(&session_id, "cancel", &tx).await;
                self.clear_session().await;
                return Ok(String::new());
            }

            // 無工具呼叫 → 此輪即為最終回覆
            if round.tool_calls.is_empty() {
                final_text = round.full_text;
                break 'outer;
            }

            // ── plan_announce 攔截（conversation_id 模式）──────────────
            if let Some(ref conv_id) = self.conversation_id {
                let has_write = round.tool_calls.iter().any(|(_, n, _)| self.is_write(n));
                let has_plan_announce = round.tool_calls.iter().any(|(_, n, _)| n == "plan_announce");

                if has_write && !has_plan_announce {
                    if !plan_announce_retried {
                        plan_announce_retried = true;
                        // 反問 LLM 要求提供 plan_announce
                        messages.push(serde_json::json!({
                            "role": "user",
                            "content": "在執行寫入操作之前，請先呼叫 plan_announce 工具，\
                                提供 confirm_phrases、cancel_phrases、interrupt_phrases 樣本短語，\
                                以及 deferred_tools（你打算執行的工具列表）。"
                        }));
                        continue 'outer;
                    } else {
                        // retry 失敗 → fallback 固定詞庫，直接建 PendingPlan
                        let deferred: Vec<DeferredTool> = round.tool_calls.iter()
                            .filter(|(_, n, _)| self.is_write(n))
                            .map(|(_, name, args)| DeferredTool { name: name.clone(), args: args.clone() })
                            .collect();
                        if !deferred.is_empty() {
                            let confirm_phrases = ["好", "確認", "沒問題", "執行", "對", "行", "繼續"];
                            let cancel_phrases  = ["不要", "算了", "取消", "停", "不用"];
                            let interrupt_phrases = ["等等", "先停", "稍等"];
                            let confirm_vecs: Vec<Vec<f32>> = futures::future::join_all(
                                confirm_phrases.iter().map(|p| (self.embed_fn)(p.to_string()))
                            ).await;
                            let cancel_vecs: Vec<Vec<f32>> = futures::future::join_all(
                                cancel_phrases.iter().map(|p| (self.embed_fn)(p.to_string()))
                            ).await;
                            let interrupt_vecs: Vec<Vec<f32>> = futures::future::join_all(
                                interrupt_phrases.iter().map(|p| (self.embed_fn)(p.to_string()))
                            ).await;
                            let c_confirm = compute_centroid(&confirm_vecs.iter().filter(|v| !v.is_empty()).cloned().collect::<Vec<_>>());
                            let c_cancel  = compute_centroid(&cancel_vecs.iter().filter(|v| !v.is_empty()).cloned().collect::<Vec<_>>());
                            let c_inter   = compute_centroid(&interrupt_vecs.iter().filter(|v| !v.is_empty()).cloned().collect::<Vec<_>>());
                            let _ = save_pending_plan(
                                &self.settings_db, conv_id, &deferred,
                                &c_confirm, &c_cancel, &c_inter,
                            ).await;
                            // 通知 LLM 計畫已記錄
                            messages.push(serde_json::json!({
                                "role": "user",
                                "content": "[系統] 操作計畫已記錄，等待使用者確認後執行。請告知使用者計畫內容並請求確認。"
                            }));
                            // 繼續讓 LLM 輸出確認請求文字
                            continue 'outer;
                        }
                    }
                }

                if has_plan_announce {
                    plan_announced = true;
                    // 找出 plan_announce tool call
                    let pa = round.tool_calls.iter().find(|(_, n, _)| n == "plan_announce");
                    if let Some((_, _, pa_args)) = pa {
                        let confirm_phrases: Vec<String> = pa_args["confirm_phrases"]
                            .as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default();
                        let cancel_phrases: Vec<String> = pa_args["cancel_phrases"]
                            .as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default();
                        let interrupt_phrases: Vec<String> = pa_args["interrupt_phrases"]
                            .as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default();

                        // deferred_tools：從 plan_announce args 或同輪寫入工具中取得
                        let deferred: Vec<DeferredTool> = if let Some(arr) = pa_args["deferred_tools"].as_array() {
                            arr.iter().filter_map(|t| {
                                let name = t["name"].as_str()?.to_string();
                                let args = t["args"].clone();
                                Some(DeferredTool { name, args })
                            }).collect()
                        } else {
                            round.tool_calls.iter()
                                .filter(|(_, n, _)| self.is_write(n))
                                .map(|(_, name, args)| DeferredTool { name: name.clone(), args: args.clone() })
                                .collect()
                        };

                        if !deferred.is_empty() {
                            // 生成 embedding centroids
                            let embed_strs = |phrases: Vec<String>| {
                                let ef = Arc::clone(&self.embed_fn);
                                async move {
                                    futures::future::join_all(
                                        phrases.into_iter().map(|p| (ef)(p))
                                    ).await
                                }
                            };
                            let confirm_vecs   = embed_strs(confirm_phrases).await;
                            let cancel_vecs    = embed_strs(cancel_phrases).await;
                            let interrupt_vecs = embed_strs(interrupt_phrases).await;

                            let non_empty = |vecs: Vec<Vec<f32>>| -> Vec<Vec<f32>> {
                                vecs.into_iter().filter(|v| !v.is_empty()).collect()
                            };
                            let c_confirm = compute_centroid(&non_empty(confirm_vecs));
                            let c_cancel  = compute_centroid(&non_empty(cancel_vecs));
                            let c_inter   = compute_centroid(&non_empty(interrupt_vecs));

                            let _ = save_pending_plan(
                                &self.settings_db, conv_id, &deferred,
                                &c_confirm, &c_cancel, &c_inter,
                            ).await;
                        }

                        // 注入 plan_announce 假結果，讓 LLM 繼續輸出確認文字
                        let tool_id = round.tool_calls.iter()
                            .find(|(_, n, _)| n == "plan_announce")
                            .map(|(id, _, _)| id.clone())
                            .unwrap_or_default();
                        if !tool_id.is_empty() {
                            let tc_json = serde_json::json!([{
                                "id": tool_id,
                                "type": "function",
                                "function": {"name": "plan_announce", "arguments": "{}"}
                            }]);
                            messages.push(serde_json::json!({
                                "role": "assistant",
                                "content": Value::Null,
                                "tool_calls": tc_json
                            }));
                            messages.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": tool_id,
                                "content": "計畫已記錄，等待使用者確認。"
                            }));
                        } else {
                            messages.push(serde_json::json!({
                                "role": "user",
                                "content": "[系統] 操作計畫已記錄，等待使用者確認後執行。"
                            }));
                        }
                        continue 'outer;
                    }
                }
            }

            // ── 顯示所有工具呼叫給前端（排除 plan_announce）─────────────
            for (_, tool_name, tool_args) in &round.tool_calls {
                if tool_name == "plan_announce" { continue; }
                let display = self.tool_display(tool_name, tool_args);
                (self.emit)("agent:tool_call".into(), Value::String(display));
            }

            // ── 寫入工具確認（批次：任一需確認則整批詢問一次）─────────
            let has_write = round.tool_calls.iter().any(|(_, n, _)| self.is_write(n));
            if has_write {
                let batch_display = round.tool_calls.iter()
                    .filter(|(_, n, _)| self.is_write(n))
                    .map(|(_, n, a)| self.tool_display(n, a))
                    .collect::<Vec<_>>()
                    .join("\n");
                (self.emit)("agent:write_request".into(), Value::String(batch_display.clone()));
                let approved = (self.confirm_write)(batch_display).await;
                if !approved {
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": "用戶拒絕了此寫入操作。"
                    }));
                    continue 'outer;
                }
            }

            // ── Planner 建立 ToolGraph，Dispatcher 執行 ───────────────
            let graph = Planner::plan(&round.tool_calls);
            let results = self.dispatcher.run(Arc::clone(&tx), graph).await?;

            // ── 記錄本輪已執行的寫入工具（用於 commit 後 emit vault:changed）
            for (_, tool_name, tool_args) in &round.tool_calls {
                if self.is_write(tool_name) {
                    committed_writes.push((tool_name.clone(), tool_args.clone()));
                }
            }

            // ── 發送 note_refs（每個工具分別檢查）────────────────────
            for ((_, tool_name, tool_args), result) in round.tool_calls.iter().zip(results.iter()) {
                let refs = self.emit_note_refs(tool_name, tool_args, result.as_str().unwrap_or(""));
                all_note_refs.extend(refs);
            }

            // ── 注入工具結果回 messages ────────────────────────────────
            let use_native = round.tool_calls.first().map(|(id, _, _)| !id.is_empty()).unwrap_or(false);
            if use_native {
                // Native OpenAI format：一個 assistant turn + 每工具一個 tool turn
                let tool_calls_json: Vec<Value> = round.tool_calls.iter()
                    .map(|(id, name, args)| serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(args).unwrap_or_default()
                        }
                    }))
                    .collect();
                messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": tool_calls_json
                }));
                for ((tool_id, _, _), result) in round.tool_calls.iter().zip(results.iter()) {
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": tool_id,
                        "content": result.as_str().unwrap_or("")
                    }));
                }
            } else {
                // 文字格式 fallback：所有結果合併成一個 user 訊息
                let combined = round.tool_calls.iter().zip(results.iter())
                    .map(|((_, name, _), result)| {
                        format!("[{}]\n{}", name, result.as_str().unwrap_or(""))
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!("工具執行結果：\n\n{}", combined)
                }));
            }
        }

        // ── note-open pending plan（搜索/讀取後儲存，供下一輪「打開它」確認）─────
        // 只在 conversation_id 模式下、且本輪無 plan_announce（避免覆蓋寫入計畫）
        if !all_note_refs.is_empty() && !plan_announced {
            if let Some(ref conv_id) = self.conversation_id {
                all_note_refs.dedup();
                let deferred = vec![DeferredTool {
                    name: "__open_note__".into(),
                    args: serde_json::json!({ "paths": all_note_refs }),
                }];
                // 使用較少短語減少嵌入延遲
                let confirm_ph  = ["好", "要", "打開", "開啟", "確認", "可以"];
                let cancel_ph   = ["不", "算了", "取消"];
                let interrupt_ph = ["等等", "先停"];
                let embed_batch = |phrases: &[&str]| {
                    let ef = Arc::clone(&self.embed_fn);
                    let ps: Vec<String> = phrases.iter().map(|p| p.to_string()).collect();
                    async move {
                        futures::future::join_all(ps.into_iter().map(|p| (ef)(p))).await
                    }
                };
                let confirm_vecs   = embed_batch(&confirm_ph).await;
                let cancel_vecs    = embed_batch(&cancel_ph).await;
                let interrupt_vecs = embed_batch(&interrupt_ph).await;
                let non_empty = |vecs: Vec<Vec<f32>>| -> Vec<Vec<f32>> {
                    vecs.into_iter().filter(|v| !v.is_empty()).collect()
                };
                let c_confirm = compute_centroid(&non_empty(confirm_vecs));
                let c_cancel  = compute_centroid(&non_empty(cancel_vecs));
                let c_inter   = compute_centroid(&non_empty(interrupt_vecs));
                let _ = save_pending_plan(
                    &self.settings_db, conv_id, &deferred,
                    &c_confirm, &c_cancel, &c_inter,
                ).await;
            }
        }

        // 所有輪次完成（或 5 輪耗盡）→ commit → emit done
        tx.commit().await?;
        self.emit_tx(&session_id, "commit", &tx).await;

        // ── vault:changed（commit 且有寫入操作，觸發前端 sidebar + editor 刷新）
        if !committed_writes.is_empty() {
            let mut creates: Vec<&str> = Vec::new();
            let mut updates: Vec<&str> = Vec::new();
            for (name, args) in &committed_writes {
                match name.as_str() {
                    "create_note" | "create_folder" => {
                        if let Some(p) = args["path"].as_str() { creates.push(p); }
                    }
                    "update_note" => {
                        if let Some(p) = args["path"].as_str() { updates.push(p); }
                    }
                    _ => {}
                }
            }
            (self.emit)("vault:changed".into(), serde_json::json!({
                "creates": creates,
                "updates": updates,
            }));
        }

        self.clear_session().await;

        (self.emit)(
            "llm:stderr".into(),
            Value::String(format!("[agent] 完成，回應 {} 字元", final_text.len())),
        );
        (self.emit)("llm:done".into(), Value::String(final_text.clone()));

        Ok(final_text)
    }

    // ── helpers ──────────────────────────────────────────────────

    /// emit tx debug 事件（prepare / commit / cancel）
    async fn emit_tx(&self, session_id: &str, kind: &str, tx: &Transaction) {
        let tools = tx.get_tools().await;
        let payload = serde_json::to_value(TxDebugEvent {
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            tools,
        })
        .unwrap_or(Value::Null);
        (self.emit)("agent:tx_debug".into(), payload);
    }

    /// 清除 current_session
    async fn clear_session(&self) {
        *self.current_session.lock().await = None;
    }

    /// 判斷是否為寫入工具（需要使用者確認）
    fn is_write(&self, name: &str) -> bool {
        matches!(name, "create_note" | "update_note" | "create_folder")
    }

    /// 工具呼叫的可讀摘要（emit 給前端顯示）
    fn tool_display(&self, name: &str, args: &Value) -> String {
        match name {
            "search_vault" => format!("🔍 搜索: \"{}\"", args["query"].as_str().unwrap_or("")),
            "list_structure" => {
                let path = args["path"].as_str().unwrap_or("");
                format!("📂 列出: {}", if path.is_empty() { "根目錄" } else { path })
            }
            "read_note" => format!("📄 讀取: {}", args["path"].as_str().unwrap_or("")),
            "create_note" => format!("✏️  建立筆記: {}", args["path"].as_str().unwrap_or("")),
            "update_note" => format!("✏️  更新筆記: {}", args["path"].as_str().unwrap_or("")),
            "create_folder" => format!("📁 建立資料夾: {}", args["path"].as_str().unwrap_or("")),
            "query_memory" => "🧠 查詢記憶".into(),
            "add_memory_rule" => format!("🧠 新增記憶規則: {}", args["pattern"].as_str().unwrap_or("")),
            _ => format!("[{}]", name),
        }
    }

    /// 發送 note_refs 事件（read_note / search_vault → 前端導航按鈕）
    /// 同時回傳路徑清單，供 run_streaming_loop 建立 note-open pending plan 使用
    fn emit_note_refs(&self, tool_name: &str, tool_args: &Value, result: &str) -> Vec<String> {
        if self.vault_path.is_empty() {
            return vec![];
        }
        let paths: Vec<String> = match tool_name {
            "read_note" => {
                if let Some(p) = tool_args["path"].as_str() {
                    let abs = std::path::PathBuf::from(&self.vault_path).join(p);
                    vec![abs.to_string_lossy().to_string()]
                } else {
                    vec![]
                }
            }
            "search_vault" => result
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
                    if line.starts_with("- **") {
                        // Extract path from `(path)` part
                        if let Some(lp) = line.rfind('(') {
                            if let Some(rp) = line[lp..].find(')') {
                                let rel = &line[lp + 1..lp + rp];
                                if rel.ends_with(".md") {
                                    let abs =
                                        std::path::PathBuf::from(&self.vault_path).join(rel);
                                    let abs_path = abs.to_string_lossy().to_string();
                                    // Extract section from `**title § section**` if present
                                    let section = if let Some(bold_end) = line.find("** (") {
                                        let bold_content = &line[4..bold_end]; // skip "- **"
                                        bold_content.find(" § ").map(|sep| bold_content[sep + 3..].to_string())
                                    } else {
                                        None
                                    };
                                    return Some(match section {
                                        Some(sec) if !sec.is_empty() => format!("{}#{}", abs_path, sec),
                                        _ => abs_path,
                                    });
                                }
                            }
                        }
                    }
                    None
                })
                .collect(),
            _ => vec![],
        };
        if !paths.is_empty() {
            (self.emit)(
                "agent:note_refs".into(),
                serde_json::to_value(paths.clone()).unwrap_or(Value::Null),
            );
        }
        paths
    }
}
