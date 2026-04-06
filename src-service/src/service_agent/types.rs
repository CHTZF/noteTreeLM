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

/// LLM 單輪串流結果
pub struct LlmRound {
    pub full_text: String,
    /// LLM 呼叫的工具列表（可能多個）：Vec<(tool_id, tool_name, tool_args)>
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

/// 通用事件 emit 回呼
/// 參數：(event_name, payload_json)
pub type EmitEventFn = Arc<dyn Fn(String, Value) + Send + Sync>;

/// 記憶預取回呼：(user_query) → 格式化記憶文字
pub type PrefetchFn = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = String> + Send>>
    + Send + Sync
>;

/// Embedding 回呼：(text) → embedding 向量（空 Vec 表示失敗）
pub type EmbedFn = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = Vec<f32>> + Send>>
    + Send + Sync
>;

/// 大型筆記摘要回呼：(file_path, user_query) → 精簡摘要
pub type SummarizeFn = Arc<
    dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Option<String>> + Send>>
    + Send + Sync
>;

/// 判斷工具名稱是否需要使用者確認的謂詞（write tools 預設需要，可擴充至特殊 non-write tools）
pub type NeedConfirmFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

// ── NewSkillSpec（create_agent 工具傳入的 skill 規格）────────────────────────

#[derive(Debug, serde::Deserialize)]
pub struct NewSkillSpec {
    pub title: String,
    pub trigger: String,
    pub behavior: String,
    #[serde(default = "default_passive")]
    pub injection_mode: String,
    #[serde(default)]
    pub need_tool_chain: bool,
    #[serde(default)]
    pub tool_chain_order: Vec<String>,
}

fn default_passive() -> String { "passive".to_string() }
