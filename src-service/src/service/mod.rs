/// Absolute safety-net round cap. Normal termination happens via stall detection
/// (repeated_calls warning) or when the LLM produces no tool call. This value
/// should rarely be reached in practice.
pub const MAX_ROUNDS: usize = 50;

// ── 基礎型別 ─────────────────────────────────────────────────────────────────
pub mod types;

// ── Harness（環境綁定 + 工具定義）────────────────────────────────────────────
pub(crate) mod harness;
pub use harness::HarnessRequestRuntime;

// ── Agent 邏輯 ───────────────────────────────────────────────────────────────
pub mod agents;

// Public entry points used by routes
pub use agents::agent::run_agent;
pub use agents::scheduled_agents::execute_scheduled_task;

/// Build a fully-populated [`HarnessRequestRuntime`] for a specific vault + account.
/// Returns `None` if the LLM URL is not yet configured.
pub async fn build_agent_runtime(
    state:      &crate::app_state::ApiState,
    vault_id:   &str,
    account_id: &str,
    session_id: Option<String>,
    conv_id:    String,
    agent_def:  serde_json::Value,
    streaming:  bool,
) -> Option<HarnessRequestRuntime> {
    use std::sync::Arc;
    use crate::service::types::{EmitEventFn, ServiceEvent};

    let llm_url       = state.daemon.llm_url.read().await.clone()?;
    let embedding_url = state.daemon.embedding_url.read().await.clone();
    let event_tx      = state.daemon.event_tx.clone();
    let emit_fn: EmitEventFn = Arc::new(move |event: String, payload: serde_json::Value| {
        let _ = event_tx.send(ServiceEvent { event, payload });
    });

    let vault_path = state.resolve_vault_path(vault_id).await;
    let kind       = agent_def["kind"].as_str().unwrap_or("").to_string();
    let session_id = Arc::new(session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()));
    let emitter    = HarnessRequestRuntime::build_emitter(&emit_fn, &session_id);
    let dispatcher = Some(HarnessRequestRuntime::build_dispatcher(emitter.as_emit_fn(), &kind));

    Some(HarnessRequestRuntime {
        db:               state.db.clone(),
        llm_url,
        embedding_url,
        vault_id:         vault_id.to_string(),
        account_id:       account_id.to_string(),
        agent_sessions:   Arc::clone(&state.daemon.agent_sessions),
        intent_centroids: Arc::clone(&state.daemon.intent_centroids),
        vault_path_cache: Arc::clone(&state.daemon.vault_path_cache),
        vault_path,
        session_id,
        conv_id:        Arc::new(conv_id),
        cancel:         Arc::new(std::sync::atomic::AtomicBool::new(false)),
        answer_channel: Arc::new(
            harness::engine::transaction::AnswerChannel::new()
        ),
        working_memory: harness::memory::working::WorkingMemory::new(),
        client:         reqwest::Client::builder()
                            .timeout(std::time::Duration::from_secs(300))
                            .build()
                            .unwrap_or_default(),
        kind,
        streaming,
        agent_def,
        write_snapshots: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        write_mtimes:    Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        emitter,
        dispatcher,
    })
}
/// Re-export harness::tools at the legacy path so routes outside this crate
/// (e.g. routes/agents/runner.rs) continue to compile without path changes.
pub use harness::tools as tools;
pub use harness::tools::vault_tools;
pub(crate) mod helpers {
    pub(crate) use super::harness::agent_def::load_agent_def;
    pub(crate) use super::harness::memory::semantic::vault_query_memory_with_limit;
    pub(crate) use super::harness::tools::llm::detect_response_framework;
    pub(crate) use super::harness::tools::skill_tools::run_skill_pass;
    #[allow(unused_imports)]
    pub(crate) use super::harness::tools::skill_tools::SkillPassResult;
}
