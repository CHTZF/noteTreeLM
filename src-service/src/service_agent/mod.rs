pub const MAX_ROUNDS: usize = 20;

// ── 基礎型別 ─────────────────────────────────────────────────────────────────
pub mod types;

// ── Harness（環境綁定 + 工具定義）────────────────────────────────────────────
pub(crate) mod harness;

// ── 執行引擎（含 context, intent_classifier） ────────────────────────────────
pub mod engine;

// ── Agent 邏輯 ───────────────────────────────────────────────────────────────
pub mod agents;

// Public entry points used by routes
pub use agents::interactive::run_agent;
pub use agents::scheduled::execute_scheduled_task;
/// Re-export harness::tools at the legacy path so routes outside this crate
/// (e.g. routes/agents/runner.rs) continue to compile without path changes.
pub use harness::tools as tools;
pub use harness::tools::vault_tools;
pub(crate) mod helpers {
    pub(crate) use super::harness::agent_def::load_agent_def;
    pub(crate) use super::harness::memory::semantic::vault_query_memory_with_limit;
    pub(crate) use super::harness::tools::vault_tools::detect_response_framework;
    pub(crate) use super::harness::tools::skill_tools::run_skill_pass;
    #[allow(unused_imports)]
    pub(crate) use super::harness::tools::skill_tools::SkillPassResult;
    pub(crate) use crate::processing::embedder::embed_text_llm;
}
