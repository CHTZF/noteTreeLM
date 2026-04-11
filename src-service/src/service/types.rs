use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::Value;

use crate::service::harness::runtime::HarnessRequestRuntime;
use crate::service::harness::governance::guard::ToolGuardSpec;
use crate::service::harness::engine::transaction::{Transaction, AnswerChannel};
use crate::service::harness::tools::skill_tools::SkillPassResult;
use crate::service::harness::memory::working::WorkingMemory;

/// Lightweight event for SSE broadcast (shared between daemon broadcast and service emit).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ServiceEvent {
    pub event: String,
    pub payload: serde_json::Value,
}

/// Per-session state for interactive agent runs.
/// Stored in `DaemonState.agent_sessions` keyed by `conv_id`.
pub struct AgentSession {
    pub session_id:     Arc<String>,
    pub conv_id:        Arc<String>,
    pub cancel:         Arc<AtomicBool>,
    pub transaction:    Option<Arc<Transaction>>,
    pub answer_channel: Arc<AnswerChannel>,
    /// Persistent evidence store shared across messages in the same conversation.
    /// `inner` (tool call records) carries over; `current_run_ids` / `recall_count`
    /// are reset at the start of each new run via `start_new_run()`.
    pub working_memory: WorkingMemory,
    /// Skills activated in the last completed run — carried forward so Gmail/Calendar
    /// tools stay active in follow-up messages without re-running semantic search.
    pub active_skills:  Option<SkillPassResult>,
}

pub type ToolFuture =
    Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;

/// Tool handler: receives the shared runtime + args, returns a boxed future.
/// Using `Arc<dyn Fn>` (not a bare fn pointer) allows both static handlers
/// and closures (e.g. eval mock tools).
pub type HandlerFn =
    Arc<dyn Fn(Arc<HarnessRequestRuntime>, Value) -> ToolFuture + Send + Sync>;

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

pub(crate) struct Tool {
    pub(crate) execute:  HandlerFn,
    pub(crate) rollback: Option<HandlerFn>,
    pub(crate) guard:    Option<ToolGuardSpec>,
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
