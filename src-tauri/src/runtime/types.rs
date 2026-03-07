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
    /// 若 LLM 呼叫工具：(tool_id, tool_name, tool_args)
    pub tool_call: Option<(String, String, Value)>,
}

/// 執行一輪 LLM 串流請求的回呼
/// 參數：(messages_json, use_tools, cancel_flag)
pub type LlmFn = Arc<
    dyn Fn(Vec<Value>, bool, Option<Arc<AtomicBool>>)
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
