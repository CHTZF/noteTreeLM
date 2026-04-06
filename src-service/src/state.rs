use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::auth::store::AuthStore;
use crate::db::SurrealDb;
use crate::service::types::EmitEventFn;
use crate::service::HarnessRequestRuntime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub port: u16,
    pub status: String,   // "running" | "stopped" | "error"
    pub model: Option<String>,
    pub updated_at: i64,
}

/// Lightweight event broadcast for SSE clients.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceEvent {
    pub event: String,
    pub payload: serde_json::Value,
}

/// Structured block reason returned by `evaluate_guard`.
/// Carries both the human-readable message and machine-readable fields so the
/// LLM can programmatically decide its next action instead of parsing text.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GuardHint {
    /// Human-readable explanation forwarded to the LLM as the tool result.
    pub message: String,
    /// The tool the agent should call next to satisfy the guard (e.g. "read_note").
    pub required_tool: Option<String>,
    /// The exact path the required_tool should be called with.
    pub required_path: Option<String>,
}

impl GuardHint {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), required_tool: None, required_path: None }
    }
    pub fn with_tool(mut self, tool: impl Into<String>, path: impl Into<String>) -> Self {
        self.required_tool = Some(tool.into());
        self.required_path = Some(path.into());
        self
    }
}

/// Guard evaluation outcome for a single tool execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "reason")]
pub enum GuardOutcome {
    /// Guard passed (or tool has no guard spec).
    Passed,
    /// Guard blocked execution; inner GuardHint carries message + required action.
    Blocked(GuardHint),
    /// Tool is explicitly exempt from guard evaluation.
    Exempt,
}

/// A single tool execution record within a session.
/// Stored in AgentSession.working_memory keyed by tool_call_id.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallRecord {
    /// Tool name (e.g. "search_vault", "read_note")
    pub name: String,
    /// Args passed to the tool
    pub args: serde_json::Value,
    /// Result returned by the tool
    pub result: serde_json::Value,
    /// Unix timestamp (seconds) when the tool call started.
    pub started_at: i64,
    /// Wall-clock execution duration in milliseconds.
    pub duration_ms: u64,
    /// Guard evaluation outcome for this call.
    pub guard_outcome: GuardOutcome,
}

/// Per-session state for interactive agent runs.
///
/// Only the fields that are accessed by external callers (cancel/confirm endpoints,
/// ask_user tool) live here. `session_id` and `conv_id` are `Arc<String>` so that
/// `AgentEnv` holds the same allocation — they are guaranteed to be identical.
pub struct AgentSession {
    /// Shared with `AgentEnv::session_id` — always the same value.
    pub session_id: Arc<String>,
    /// Shared with `AgentEnv::conv_id` — always the same value.
    pub conv_id: Arc<String>,
    /// Shared with `AgentEnv::cancel` — set true to abort the agent loop.
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
    pub transaction: Option<Arc<crate::service::harness::engine::transaction::Transaction>>,
    /// Channel for the `ask_user` tool / Step-0b resume flow.
    /// Shared via `Arc` so Step-0b only needs `sessions.get()`, not `get_mut()`.
    pub answer_channel: Arc<crate::service::harness::engine::transaction::AnswerChannel>,
}

#[derive(Clone)]
pub struct DaemonState {
    pub servers: Arc<RwLock<Vec<ServerInfo>>>,
    pub sqlite: crate::db::sqlite::SqliteConn,
    pub embedding_url: Arc<RwLock<Option<String>>>,
    pub llm_url: Arc<RwLock<Option<String>>>,
    /// Public tunnel URL from cloudflared (None if cloudflared not running)
    pub tunnel_url: Arc<RwLock<Option<String>>>,
    /// Broadcast channel for server-sent events (capacity 64).
    pub event_tx: tokio::sync::broadcast::Sender<ServiceEvent>,
    /// Active interactive agent sessions keyed by session_id.
    pub agent_sessions: Arc<Mutex<HashMap<String, AgentSession>>>,
    /// Intent centroid cache (confirm / cancel / interrupt embeddings).
    pub intent_centroids: Arc<Mutex<Option<(Vec<f32>, Vec<f32>, Vec<f32>)>>>,
    /// vault_id → vault_path in-process cache (avoids a DB round-trip on every note read).
    pub vault_path_cache: Arc<std::sync::RwLock<HashMap<String, String>>>,
}

impl DaemonState {
    pub fn new_with_data_dir(data_dir: &Path) -> Self {
        let sqlite = crate::db::sqlite::init_sqlite(&data_dir.join("search.db"))
            .expect("SQLite FTS5 init failed");
        let (event_tx, _) = tokio::sync::broadcast::channel(64);
        Self {
            servers: Arc::new(RwLock::new(Vec::new())),
            sqlite,
            embedding_url: Arc::new(RwLock::new(None)),
            llm_url: Arc::new(RwLock::new(None)),
            tunnel_url: Arc::new(RwLock::new(None)),
            event_tx,
            agent_sessions: Arc::new(Mutex::new(HashMap::new())),
            intent_centroids: Arc::new(Mutex::new(None)),
            vault_path_cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Best-effort broadcast — ignores send errors (no subscribers).
    pub fn emit(&self, event: impl Into<String>, payload: serde_json::Value) {
        let _ = self.event_tx.send(ServiceEvent {
            event: event.into(),
            payload,
        });
    }
}

// ── ApiState ──────────────────────────────────────────────────────────────────

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
                crate::service::harness::engine::transaction::AnswerChannel::new()
            ),
            working_memory: crate::service::harness::memory::working::WorkingMemory::new(),
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
        if let Ok(cache) = self.daemon.vault_path_cache.read() {
            if let Some(path) = cache.get(vault_id) {
                return path.clone();
            }
        }
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
        if !path.is_empty() {
            if let Ok(mut cache) = self.daemon.vault_path_cache.write() {
                cache.insert(vault_id.to_string(), path.clone());
            }
        }
        path
    }
}
