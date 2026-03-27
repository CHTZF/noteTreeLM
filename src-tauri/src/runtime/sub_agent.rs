// runtime/sub_agent.rs
// 輕量 Sub-Agent：供 Main Agent 透過 spawn_sub_agent 工具委派子任務。
//
// 特性：
// - 無 pending plan、無寫入確認（自動允許）、無記憶預取
// - 最多 MAX_ROUNDS 輪工具呼叫，避免 context 爆炸
// - 事件以 "sub_agent:*" 命名空間 emit，供 Debug tab 顯示子 agent 樹

use std::sync::Arc;

use serde_json::Value;

use super::dispatcher::Dispatcher;
use super::planner::Planner;
use super::tool_registry::ToolRegistry;
use super::transaction::Transaction;
use super::types::{EmitEventFn, LlmFn};

const MAX_ROUNDS: usize = 5;

/// sub-agent 種類
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubAgentKind {
    Search,   // 搜尋型：search_vault, read_note, list_structure, query_memory
    Write,    // 寫入型：create_note, update_note, create_folder
    Research, // 研究型：web_search, search_vault
    Memory,   // 記憶型：query_memory, list_recent_conversations
    Custom,   // 自訂：不過濾，使用全部工具
}

impl SubAgentKind {
    pub fn from_str(s: &str) -> Self {
        match s {
            "search"   => Self::Search,
            "write"    => Self::Write,
            "research" => Self::Research,
            "memory"   => Self::Memory,
            _          => Self::Custom,
        }
    }

    /// 回傳該種類需要的工具名稱清單（None = 不過濾，使用全部）
    pub fn tool_filter(&self) -> Option<&'static [&'static str]> {
        match self {
            Self::Search   => Some(&["search_vault", "read_note", "open_note", "list_structure", "query_memory"]),
            Self::Write    => Some(&["create_note", "update_note", "create_folder"]),
            Self::Research => Some(&["web_search", "search_vault", "read_note"]),
            Self::Memory   => Some(&["query_memory", "list_recent_conversations"]),
            Self::Custom   => None,
        }
    }
}

/// 執行一個 Sub-Agent，完成 task 後返回最終文字結果。
///
/// # 參數
/// - `sub_session_id`: 此 sub-agent 的唯一識別（由呼叫端生成）
/// - `parent_session_id`: 父 Main Agent 的 session_id
/// - `kind_str`: "search" / "write" / "research" / "memory" / "custom"
/// - `task`: 子任務描述（成為 sub-agent 的 user message）
/// - `context`: 可選背景資訊（注入 system prompt）
/// - `tools_json`: 全部可用工具的 JSON Value（根據 kind 過濾）
/// - `registry`: 工具執行 registry（與 Main Agent 共用）
/// - `llm_fn`: LLM 呼叫回呼
/// - `emit`: 事件 emit 回呼
pub async fn run_sub_agent(
    sub_session_id: String,
    parent_session_id: String,
    kind_str: &str,
    task: &str,
    context: &str,
    tools_json: Value,
    registry: Arc<ToolRegistry>,
    llm_fn: LlmFn,
    emit: EmitEventFn,
) -> String {
    let kind = SubAgentKind::from_str(kind_str);

    // 過濾工具列表
    let filtered_tools: Value = if let Some(allowed) = kind.tool_filter() {
        let arr = tools_json.as_array().cloned().unwrap_or_default();
        let filtered: Vec<Value> = arr
            .into_iter()
            .filter(|t| {
                let name = t["function"]["name"].as_str().unwrap_or("");
                allowed.contains(&name)
            })
            .collect();
        Value::Array(filtered)
    } else {
        tools_json
    };

    let system_content = if context.is_empty() {
        format!(
            "你是一個專業的 {kind_str} 助理，負責完成指定子任務。\n\
             規則：\n\
             1. 必須實際呼叫工具執行任務；禁止假裝已完成或虛構結果。\n\
             2. 若搜尋無結果，直接回報找不到，不可捏造內容或筆記名稱。\n\
             3. 完成後簡潔回傳結果，不加多餘說明。"
        )
    } else {
        format!(
            "你是一個專業的 {kind_str} 助理，負責完成指定子任務。\n\
             背景資訊：{context}\n\
             規則：\n\
             1. 必須實際呼叫工具執行任務；禁止假裝已完成或虛構結果。\n\
             2. 若搜尋無結果，直接回報找不到，不可捏造內容或筆記名稱。\n\
             3. 完成後簡潔回傳結果，不加多餘說明。"
        )
    };

    let mut messages: Vec<Value> = vec![
        serde_json::json!({"role": "system", "content": system_content}),
        serde_json::json!({"role": "user", "content": task}),
    ];

    emit(
        "sub_agent:start".into(),
        serde_json::json!({
            "sub_session_id": sub_session_id,
            "parent_session_id": parent_session_id,
            "kind": kind_str,
            "task": task,
        }),
    );

    let dispatcher = Dispatcher::new(Arc::clone(&registry));
    let tx = Arc::new(Transaction::new());
    let _ = tx.prepare().await;

    let mut final_text = String::new();

    for round in 0..MAX_ROUNDS {
        let tools_opt = if filtered_tools.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            Some(filtered_tools.clone())
        } else {
            None
        };

        let llm_result = match llm_fn(messages.clone(), tools_opt, None).await {
            Ok(r) => r,
            Err(e) => {
                emit(
                    "sub_agent:error".into(),
                    serde_json::json!({ "sub_session_id": sub_session_id, "error": e }),
                );
                return format!("[sub-agent 錯誤] {}", e);
            }
        };

        final_text = llm_result.full_text.clone();

        if llm_result.tool_calls.is_empty() {
            break;
        }

        // Emit tool display events
        for (_, name, args) in &llm_result.tool_calls {
            emit(
                "sub_agent:tool_call".into(),
                serde_json::json!({
                    "sub_session_id": sub_session_id,
                    "parent_session_id": parent_session_id,
                    "kind": kind_str,
                    "display": format!("[{kind_str}] {name}"),
                    "args": args,
                }),
            );
        }

        // 執行工具
        let tool_graph = Planner::plan(&llm_result.tool_calls);
        let results = match dispatcher.run(Arc::clone(&tx), tool_graph).await {
            Ok(r) => r,
            Err(e) => {
                emit(
                    "sub_agent:error".into(),
                    serde_json::json!({ "sub_session_id": sub_session_id, "error": e }),
                );
                return format!("[sub-agent tool error] {}", e);
            }
        };

        // 追加 assistant 訊息 + tool results
        let tc_json: Vec<Value> = llm_result.tool_calls.iter().map(|(id, name, args)| {
            serde_json::json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args.to_string()}
            })
        }).collect();

        messages.push(serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": tc_json,
        }));

        for ((tool_id, name, _), result) in llm_result.tool_calls.iter().zip(results.iter()) {
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": tool_id,
                "name": name,
                "content": result.as_str().unwrap_or(&result.to_string()),
            }));
        }

        if round == MAX_ROUNDS - 1 {
            final_text = format!("[達最大輪數 {MAX_ROUNDS}，部分結果] {}", final_text);
        }
    }

    let _ = tx.commit().await;

    emit(
        "sub_agent:done".into(),
        serde_json::json!({
            "sub_session_id": sub_session_id,
            "parent_session_id": parent_session_id,
            "kind": kind_str,
            "result_preview": final_text.chars().take(200).collect::<String>(),
        }),
    );

    final_text
}
