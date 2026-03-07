use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::Value;

pub type ToolFuture =
    Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;

pub type ToolFn =
    Arc<dyn Fn(Value) -> ToolFuture + Send + Sync>;

/// 前端 debug 區塊用的 transaction 狀態事件
#[derive(Debug, Clone, serde::Serialize)]
pub struct TxDebugEvent {
    pub session_id: String,
    /// "prepare" | "commit" | "cancel"
    pub kind: String,
    /// 此 transaction 中已執行的工具清單
    pub tools: Vec<String>,
}

#[derive(Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

pub struct Tool {
    pub execute: ToolFn,
    pub rollback: Option<ToolFn>,
}

// ── Agent 回呼型別 ─────────────────────────────────────────────────────────

/// LLM 單輪串流結果（send_streaming_request 的高階封裝）
pub struct LlmRound {
    pub full_text: String,
    /// LLM 呼叫的工具列表（可能多個）：Vec<(tool_id, tool_name, tool_args)>
    /// 空 Vec 表示無工具呼叫（最終回覆）
    pub tool_calls: Vec<(String, String, Value)>,
}

/// 執行一輪 LLM 串流請求的回呼
/// 參數：(messages_json, tools（None=不傳工具）, cancel_flag)
pub type LlmFn = Arc<
    dyn Fn(Vec<Value>, Option<Value>, Option<Arc<AtomicBool>>)
        -> Pin<Box<dyn Future<Output = Result<LlmRound, String>> + Send>>
    + Send + Sync
>;

/// 寫入工具確認回呼：(display_text) → approved
pub type ConfirmWriteFn = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = bool> + Send>>
    + Send + Sync
>;

/// 通用事件 emit 回呼（避免直接依賴 tauri::AppHandle）
/// 參數：(event_name, payload_json)
pub type EmitEventFn = Arc<dyn Fn(String, Value) + Send + Sync>;

/// 記憶預取回呼：(user_query) → 格式化記憶文字（純 Rust DB 查詢，空字串=無結果）
/// 在 Intent::Memory 路徑用於注入初始種子上下文，不含 LLM fallback
pub type PrefetchFn = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = String> + Send>>
    + Send + Sync
>;

