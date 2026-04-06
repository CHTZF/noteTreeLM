use std::sync::Arc;
use serde_json::Value;
use crate::auth::store::AuthStore;
use crate::db::SurrealDb;
use crate::state::{DaemonState, ServiceEvent};
use crate::service_agent::types::EmitEventFn;
use crate::service_agent::HarnessRequestRuntime;

/// Shared state for all HTTP API route handlers (both :7787 and :7788)
#[derive(Clone)]
pub struct ApiState {
    pub auth: AuthStore,
    pub db: SurrealDb,
    pub daemon: DaemonState,
}

impl ApiState {
    pub fn new(auth: AuthStore, db: SurrealDb, daemon: DaemonState) -> Self {
        Self { auth, db, daemon }
    }

    /// Construct a fully-populated `HarnessRequestRuntime` for a specific vault + account.
    /// Returns `None` if the LLM URL is not yet configured.
    pub async fn agent_runtime(
        &self,
        vault_id:   &str,
        account_id: &str,
        session_id: Option<String>,
        conv_id:    String,
        agent_def:  Value,
        streaming:  bool,
    ) -> Option<HarnessRequestRuntime> {
        let llm_url       = self.daemon.llm_url.read().await.clone()?;
        let embedding_url = self.daemon.embedding_url.read().await.clone();
        let event_tx      = self.daemon.event_tx.clone();
        let emit_fn: EmitEventFn = Arc::new(move |event: String, payload: Value| {
            let _ = event_tx.send(ServiceEvent { event, payload });
        });

        let vault_path = self.resolve_vault_path(vault_id).await;
        let kind       = agent_def["kind"].as_str().unwrap_or("").to_string();
        let session_id = Arc::new(session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()));
        let emitter    = HarnessRequestRuntime::build_emitter(&emit_fn, &session_id);
        let dispatcher = Some(HarnessRequestRuntime::build_dispatcher(emitter.as_emit_fn(), &kind));

        Some(HarnessRequestRuntime {
            db:               self.db.clone(),
            llm_url,
            embedding_url,
            vault_id:         vault_id.to_string(),
            account_id:       account_id.to_string(),
            agent_sessions:   Arc::clone(&self.daemon.agent_sessions),
            intent_centroids: Arc::clone(&self.daemon.intent_centroids),
            vault_path_cache: Arc::clone(&self.daemon.vault_path_cache),
            vault_path,
            session_id,
            conv_id:        Arc::new(conv_id),
            cancel:         Arc::new(std::sync::atomic::AtomicBool::new(false)),
            answer_channel: Arc::new(
                crate::service_agent::harness::engine::transaction::AnswerChannel::new()
            ),
            working_memory: crate::service_agent::harness::memory::working::WorkingMemory::new(),
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

    /// Resolve vault filesystem path from vault_id.
    /// Checks the in-process cache first; falls back to DB query.
    /// Returns empty string if vault_id is empty or vault not found.
    pub async fn resolve_vault_path(&self, vault_id: &str) -> String {
        if vault_id.is_empty() { return String::new(); }
        // Fast path: cache
        if let Ok(cache) = self.daemon.vault_path_cache.read() {
            if let Some(path) = cache.get(vault_id) {
                return path.clone();
            }
        }
        // DB lookup
        #[derive(serde::Deserialize)]
        struct Row { path: String }
        let path: String = self.db
            .query("SELECT path FROM vaults WHERE vault_id = $vid LIMIT 1")
            .bind(("vid", vault_id.to_string()))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| r.path)
            .unwrap_or_default();
        // Populate cache
        if !path.is_empty() {
            if let Ok(mut cache) = self.daemon.vault_path_cache.write() {
                cache.insert(vault_id.to_string(), path.clone());
            }
        }
        path
    }
}
