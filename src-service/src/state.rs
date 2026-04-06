use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

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
    pub transaction: Option<Arc<crate::service_agent::engine::transaction::Transaction>>,
    /// Channel for the `ask_user` tool / Step-0b resume flow.
    /// Shared via `Arc` so Step-0b only needs `sessions.get()`, not `get_mut()`.
    pub answer_channel: Arc<crate::service_agent::engine::transaction::AnswerChannel>,
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
