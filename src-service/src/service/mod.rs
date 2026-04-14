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
    state:       &crate::app_state::ApiState,
    vault_id:    &str,
    account_id:  &str,
    session_id:  Option<String>,
    conv_id:     String,
    agent_def:   serde_json::Value,
    streaming:   bool,
    ui_language: Option<&str>,
    source_type: Option<String>,
    source_id:   Option<String>,
) -> Option<HarnessRequestRuntime> {
    use std::sync::Arc;
    use crate::service::types::{EmitEventFn, ServiceEvent};
    use harness::prompt::Locale;

    let llm_url       = state.daemon.llm_url.clone();
    let draft_llm_url = state.daemon.draft_llm_url.clone();
    let embedding_url = Some(state.daemon.embedding_url.clone());
    let event_tx      = state.daemon.event_tx.clone();
    let emit_fn: EmitEventFn = Arc::new(move |event: String, payload: serde_json::Value| {
        let _ = event_tx.send(ServiceEvent { event, payload });
    });

    let vault_path = state.resolve_vault_path(vault_id).await;
    let kind       = agent_def["kind"].as_str().unwrap_or("").to_string();

    // Derive model metadata from the stored model path (no extra DB keys needed).
    let (native_think, context_budget) = {
        #[derive(serde::Deserialize)]
        struct Row { value: String }
        let model_path: String = state.db
            .query("SELECT `value` FROM `settings` WHERE `key` = $key LIMIT 1")
            .bind(("key", "llm_model_path"))
            .await.ok()
            .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| r.value)
            .unwrap_or_default();
        let meta = model_meta(&model_path);
        (meta.native_think, harness::context::ContextBudget::from_context_size(meta.ctx_size))
    };
    let session_id = Arc::new(session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()));
    let emitter    = HarnessRequestRuntime::build_emitter(&emit_fn, &session_id);
    let dispatcher = Some(HarnessRequestRuntime::build_dispatcher(emitter.as_emit_fn(), &kind));

    Some(HarnessRequestRuntime {
        db:               state.db.clone(),
        llm_url,
        draft_llm_url,
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
        source_type,
        source_id,
        locale: ui_language.map(Locale::from_tag).unwrap_or_default(),
        agent_def,
        write_snapshots: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        write_mtimes:    Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        context_budget,
        native_think,
        active_note:   None,
        selection:     None,
            platform:    None,
        active_skills: Arc::new(tokio::sync::RwLock::new(None)),
        emitter,
        dispatcher,
        context: harness::context::ContextBuffer::new(),
    })
}
/// Static metadata for a known model, derived from its filename.
pub(crate) struct ModelMeta {
    /// Native context window in tokens — used for `--ctx-size` and `ContextBudget`.
    pub ctx_size:     usize,
    /// True when the model produces `<think>...</think>` blocks natively (e.g. Qwen3.5).
    /// When true, the think tool is not injected regardless of the agent's `enable_think`.
    pub native_think: bool,
}

/// Look up static metadata for a model by its file path.
/// Matches on the filename portion only. Falls back to safe defaults for unknown models.
pub(crate) fn model_meta(model_path: &str) -> ModelMeta {
    let filename = std::path::Path::new(model_path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("");

    // (filename, ctx_size_tokens, native_think)
    const TABLE: &[(&str, usize, bool)] = &[
        ("Qwen2.5-1.5B-Instruct-Q4_K_M.gguf", 32_768, false),
        ("Qwen2.5-3B-Instruct-Q4_K_M.gguf",   32_768, false),
        ("Qwen2.5-7B-Instruct-Q4_K_M.gguf",   32_768, false),
        ("Qwen2.5-14B-Instruct-Q4_K_M.gguf",  32_768, false),
        ("Qwen3.5-0.8B-Q4_K_M.gguf",          32_768, true),
        ("Qwen3.5-9B-Q4_K_M.gguf",            32_768, true),
        ("Qwen3.5-9B-Q6_K.gguf",              32_768, true),
        ("google_gemma-4-E4B-it-Q4_K_M.gguf", 131_072, true),
    ];
    TABLE.iter()
        .find(|(name, _, _)| filename.eq_ignore_ascii_case(name))
        .map(|&(_, ctx, think)| ModelMeta { ctx_size: ctx, native_think: think })
        .unwrap_or(ModelMeta { ctx_size: 8_192, native_think: false })
}

/// Convenience wrapper — returns only the context window size.
pub(crate) fn ctx_size_for_model(model_path: &str) -> usize {
    model_meta(model_path).ctx_size
}

/// Re-export harness::tools at the legacy path so routes outside this crate
/// (e.g. routes/agents/runner.rs) continue to compile without path changes.
pub use harness::tools as tools;
pub use harness::tools::vault_tools;
pub(crate) mod helpers {
    pub(crate) use super::harness::agent_def::load_agent_def;
    pub(crate) use super::harness::tools::llm::detect_response_framework;
    #[allow(unused_imports)]
    pub(crate) use super::harness::tools::skill_tools::SkillPassResult;
}
