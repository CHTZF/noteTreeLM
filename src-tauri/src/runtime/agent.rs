// agent.rs
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::db::surreal::SurrealDb;

use super::dispatcher::Dispatcher;
use super::intent_classifier::{Intent, IntentClassifier};
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
    /// 記憶預取回呼（注入 system prompt 初始種子；None = 無 vault DB）
    prefetch_memory: Option<PrefetchFn>,
    /// Embedding 回呼（用於 plan_announce centroid 計算）
    embed_fn: EmbedFn,
    /// DB（用於 pending_plans CRUD）
    db: SurrealDb,
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
        db: SurrealDb,
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
            db,
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
            let plan = load_pending_plan(&self.db, conv_id).await
                .unwrap_or(None);

            if let Some(pending) = plan {
                // TTL 檢查：超過 24h 自動取消
                let age = chrono::Utc::now().timestamp() - pending.created_at;
                if age > 86400 {
                    let _ = delete_pending_plan(&self.db, conv_id).await;
                    // 通知 LLM 計畫已過期，繼續正常處理
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": "[系統] 先前的操作計畫已過期（超過 24 小時）自動取消。"
                    }));
                } else {
                    // 用 embedding 分類 intent
                    let intent = self.intent_classifier.classify_with_embedding(
                        &user_input,
                        &self.embed_fn,
                    ).await;

                    // 無論任何 intent 都清除 pending plan
                    let _ = delete_pending_plan(&self.db, conv_id).await;

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

        // Context overflow 閾值：約 6000 tokens（粗估 CJK 約 2 char/token，英文 4 char/token）
        // 超過時保留 system + 最近 4 條訊息，其餘壓縮為摘要注入
        const OVERFLOW_CHARS: usize = 20_000;

        'outer: for _round in 0..5 {
            // 每輪前檢查取消旗標
            if cancel.load(Ordering::Relaxed) {
                let _ = tx.cancel().await;
                self.emit_tx(&session_id, "cancel", &tx).await;
                self.clear_session().await;
                return Ok(String::new());
            }

            // ── Context overflow 偵測 ─────────────────────────────────
            {
                let total_chars: usize = messages.iter()
                    .map(|m| m["content"].as_str().unwrap_or("").len())
                    .sum();
                if total_chars > OVERFLOW_CHARS && messages.len() > 6 {
                    // 壓縮中間歷史：保留 system + 最後 4 條，其餘替換為摘要
                    let system_msg = messages.first().cloned();
                    let tail: Vec<Value> = messages.iter().rev().take(4).cloned().collect::<Vec<_>>()
                        .into_iter().rev().collect();
                    let middle: Vec<Value> = messages
                        .iter()
                        .skip(1)
                        .take(messages.len().saturating_sub(5))
                        .cloned()
                        .collect();
                    let summary_text = middle.iter()
                        .filter_map(|m| {
                            let role = m["role"].as_str().unwrap_or("");
                            let content = m["content"].as_str().unwrap_or("");
                            if content.is_empty() { None }
                            else { Some(format!("[{}]: {}…", role, &content.chars().take(100).collect::<String>())) }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    let mut new_messages = Vec::new();
                    if let Some(sys) = system_msg { new_messages.push(sys); }
                    if !summary_text.is_empty() {
                        new_messages.push(serde_json::json!({
                            "role": "system",
                            "content": format!("[對話歷史摘要（{} 條訊息已壓縮）]\n{}", middle.len(), summary_text)
                        }));
                    }
                    new_messages.extend(tail);
                    messages = new_messages;

                    (self.emit)("sub_agent:context_overflow".into(), serde_json::json!({
                        "session_id": session_id,
                        "compressed_messages": middle.len(),
                        "remaining_chars": messages.iter().map(|m| m["content"].as_str().unwrap_or("").len()).sum::<usize>(),
                    }));
                }
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
                            let _ = save_pending_plan(
                                &self.db, conv_id, &deferred,
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
                            let _ = save_pending_plan(
                                &self.db, conv_id, &deferred,
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
                let _ = save_pending_plan(&self.db, conv_id, &deferred).await;
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
        matches!(name,
            "create_note" | "update_note" | "append_to_note" | "create_folder" |
            "delete_note" | "delete_folder" | "move_note"
        )
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
            "list_recent_conversations" => format!("🔎 觀察最近 {} 段對話", args["limit"].as_u64().unwrap_or(10)),
            "create_agent_skill" => format!("⚡ 建立技能：{}", args["title"].as_str().unwrap_or("")),
            "call_agent" => format!("🤖 委派給 {}：{}", args["target"].as_str().unwrap_or("agent"), args["task"].as_str().unwrap_or("")),
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
