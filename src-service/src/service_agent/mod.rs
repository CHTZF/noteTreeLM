pub const MAX_ROUNDS: usize = 20;

// ── 基礎型別 ─────────────────────────────────────────────────────────────────
pub mod types;

// ── Harness（環境綁定 + 工具定義）────────────────────────────────────────────
pub(crate) mod harness;

// ── 執行引擎（含 context, intent_classifier） ────────────────────────────────
pub mod engine;

// ── 工具實作（含 skill_tools） ───────────────────────────────────────────────
pub mod tools;

// ── Agent 邏輯 ───────────────────────────────────────────────────────────────
pub mod agents;

// Public entry points used by routes
pub use agents::interactive::run_agent;
pub use agents::scheduled::execute_scheduled_task;
pub use tools::vault_tools;
// Compatibility re-export so existing callers (routes, interactive.rs) need no path changes.
pub(crate) mod helpers {
    pub(crate) use super::engine::context::{load_agent_def, vault_query_memory_with_limit, detect_response_framework};
    pub(crate) use super::tools::skill_tools::run_skill_pass;
    #[allow(unused_imports)]
    pub(crate) use super::tools::skill_tools::SkillPassResult;
    pub(crate) use crate::processing::embedder::embed_text_llm;
}
