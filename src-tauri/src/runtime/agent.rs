// agent.rs
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::dispatcher::Dispatcher;
use super::graph::ToolGraph;
use super::intent_classifier::{Intent, IntentClassifier};
use super::transaction::Transaction;
use super::types::{ConfirmWriteFn, EmitEventFn, LlmFn, ToolCall, TxDebugEvent};

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
        let intent = self.intent_classifier.classify(&user_input).await;

        match intent {
            // ── 取消 / 中斷 ───────────────────────────────────────────
            Intent::Cancel | Intent::Interrupt => {
                self.stream_cancel.store(true, Ordering::Relaxed);
                (self.emit)("agent:cancelled".into(), Value::Null);
                return Ok(String::new());
            }

            // ── 確認（行內確認由 confirm_write 閉包處理；此處略過）────
            Intent::Confirm => {
                return Ok(String::new());
            }

            // ── 工具使用 / 對話 → 多輪 LLM loop ──────────────────────
            Intent::ToolUse | Intent::Chat => {
                // 重置取消旗標，生成新 session_id
                self.stream_cancel.store(false, Ordering::Relaxed);
                let session_id = Uuid::new_v4().to_string();
                *self.current_session.lock().await = Some(session_id.clone());

                // 建立 Transaction → prepare → emit
                let tx = Arc::new(Transaction::new());
                tx.prepare().await?;
                self.emit_tx(&session_id, "prepare", &tx).await;

                let cancel = Arc::clone(&self.stream_cancel);
                let mut final_text = String::new();

                // 多輪 LLM loop（最多 5 輪工具調用）
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
                        use_tools,
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
                    let Some((tool_id, tool_name, tool_args)) = round.tool_call else {
                        final_text = round.full_text;
                        break 'outer;
                    };

                    // 顯示工具呼叫給前端
                    let display = self.tool_display(&tool_name, &tool_args);
                    (self.emit)("agent:tool_call".into(), Value::String(display.clone()));

                    // 寫入工具：emit write_request → 等待 confirm_write 回呼
                    let approved = if self.is_write(&tool_name) {
                        (self.emit)("agent:write_request".into(), Value::String(display.clone()));
                        (self.confirm_write)(display).await
                    } else {
                        true
                    };

                    if !approved {
                        // 用戶拒絕，注入拒絕訊息後繼續下一輪
                        messages.push(serde_json::json!({
                            "role": "user",
                            "content": "用戶拒絕了此寫入操作。"
                        }));
                        continue 'outer;
                    }

                    // 執行工具 via Dispatcher（單節點 ToolGraph，有 Transaction + rollback）
                    let mut graph = ToolGraph::new();
                    graph.add_node(
                        tool_id.clone(),
                        ToolCall {
                            id: tool_id.clone(),
                            name: tool_name.clone(),
                            args: tool_args.clone(),
                        },
                        vec![],
                    );
                    let results = self.dispatcher.run(Arc::clone(&tx), graph).await?;

                    let tool_result = results
                        .first()
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    // 發送 note_refs（read_note / search_vault）
                    self.maybe_emit_note_refs(&tool_name, &tool_args, &tool_result);

                    // 注入工具結果回 messages（native tool_calls 格式優先）
                    if !tool_id.is_empty() {
                        messages.push(serde_json::json!({
                            "role": "assistant",
                            "content": Value::Null,
                            "tool_calls": [{
                                "id": &tool_id,
                                "type": "function",
                                "function": {
                                    "name": &tool_name,
                                    "arguments": serde_json::to_string(&tool_args).unwrap_or_default()
                                }
                            }]
                        }));
                        messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": &tool_id,
                            "content": &tool_result
                        }));
                    } else {
                        // text-fallback format
                        messages.push(serde_json::json!({
                            "role": "user",
                            "content": format!("工具執行結果：\n\n[{}]\n{}", tool_name, tool_result)
                        }));
                    }
                }

                // 所有輪次完成（或 5 輪耗盡）→ commit → emit done
                tx.commit().await?;
                self.emit_tx(&session_id, "commit", &tx).await;
                self.clear_session().await;

                (self.emit)(
                    "llm:stderr".into(),
                    Value::String(format!("[agent] 完成，回應 {} 字元", final_text.len())),
                );
                (self.emit)("llm:done".into(), Value::String(final_text.clone()));

                Ok(final_text)
            }
        }
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
            _ => format!("[{}]", name),
        }
    }

    /// 發送 note_refs 事件（read_note / search_vault → 前端導航按鈕）
    fn maybe_emit_note_refs(&self, tool_name: &str, tool_args: &Value, result: &str) {
        if self.vault_path.is_empty() {
            return;
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
                        if let Some(lp) = line.rfind('(') {
                            if let Some(rp) = line[lp..].find(')') {
                                let rel = &line[lp + 1..lp + rp];
                                if rel.ends_with(".md") {
                                    let abs =
                                        std::path::PathBuf::from(&self.vault_path).join(rel);
                                    return Some(abs.to_string_lossy().to_string());
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
                serde_json::to_value(paths).unwrap_or(Value::Null),
            );
        }
    }
}
