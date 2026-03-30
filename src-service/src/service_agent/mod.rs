pub const MAX_ROUNDS: usize = 20;

// ── 基礎型別 ─────────────────────────────────────────────────────────────────
pub mod types;

// ── 執行引擎 ─────────────────────────────────────────────────────────────────
pub mod tool_registry;
pub mod transaction;
pub mod graph;
pub mod planner;
pub mod executor;
pub mod dispatcher;

// ── 意圖分類 ─────────────────────────────────────────────────────────────────
pub mod intent_classifier;

// ── Agent 工廠 / 系統 Agent ──────────────────────────────────────────────────
pub mod agent_factory;
pub mod system_agent_svc;

// ── 工具實作 / Agent 邏輯 ────────────────────────────────────────────────────
mod helpers;
mod memory_tools;
mod vault_tools;
mod scheduled;
mod interactive;
mod live_chat;

pub use scheduled::execute_scheduled_task;
pub use interactive::run_interactive_agent;
pub use live_chat::run_live_chat_agent;
