pub const MAX_ROUNDS: usize = 20;

// ── 基礎型別 ─────────────────────────────────────────────────────────────────
pub mod types;

// ── 輔助函式 ─────────────────────────────────────────────────────────────────
pub(crate) mod helpers;

// ── 意圖分類 ─────────────────────────────────────────────────────────────────
pub mod intent_classifier;

// ── 執行引擎 ─────────────────────────────────────────────────────────────────
pub mod engine;

// ── 工具實作 ─────────────────────────────────────────────────────────────────
pub mod tools;

// ── Agent 邏輯 ───────────────────────────────────────────────────────────────
pub mod agents;

// Public entry points used by routes
pub use agents::interactive::run_interactive_agent;
pub use agents::interactive::run_agent;
pub use agents::live_chat::run_live_chat_agent;
pub use agents::scheduled::execute_scheduled_task;
