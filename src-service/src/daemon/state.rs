use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::config::Config;
use crate::service::types::{AgentSession, ServiceEvent};

#[derive(Clone)]
pub struct DaemonState {
    pub sqlite: crate::db::sqlite::SqliteConn,
    /// Public tunnel URL from cloudflared (None if cloudflared not running)
    pub tunnel_url: Arc<RwLock<Option<String>>>,
    /// Broadcast channel for server-sent events (capacity 64).
    pub event_tx: tokio::sync::broadcast::Sender<ServiceEvent>,
    /// Shutdown signal — sent once when the server is stopping.
    /// WebSocket handlers subscribe and close their connections on receipt.
    pub ws_shutdown_tx: tokio::sync::broadcast::Sender<()>,
    /// Active interactive agent sessions keyed by session_id.
    pub agent_sessions: Arc<Mutex<HashMap<String, AgentSession>>>,
    /// Intent centroid cache (confirm / cancel / interrupt embeddings).
    pub intent_centroids: Arc<Mutex<Option<(Vec<f32>, Vec<f32>, Vec<f32>)>>>,
    /// vault_id → vault_path in-process cache (avoids a DB round-trip on every note read).
    pub vault_path_cache: Arc<std::sync::RwLock<HashMap<String, String>>>,

    /// Shared HTTP client — connection-pool reuse across all whisper / LLM requests.
    pub http_client: reqwest::Client,

    // ── 固定 URL（從 config 計算，runtime 不變）─────────────────────────────
    pub llm_url: String,
    pub embedding_url: String,
    pub whisper_url: String,

    // ── 子進程 handles（生命週期管理）──────────────────────────────────────
    pub whisper_server: Arc<Mutex<Option<tokio::process::Child>>>,
    pub whisper_start_lock: Arc<Mutex<()>>,
    pub whisper_user_stopped: Arc<AtomicBool>,
    pub llama_server: Arc<Mutex<Option<tokio::process::Child>>>,
    pub llama_start_lock: Arc<Mutex<()>>,
    pub llama_user_stopped: Arc<AtomicBool>,
    pub embedding_server: Arc<Mutex<Option<tokio::process::Child>>>,
    pub embedding_start_lock: Arc<Mutex<()>>,
    pub embedding_user_stopped: Arc<AtomicBool>,
}

impl DaemonState {
    pub fn new(data_dir: &Path, config: &Config) -> Self {
        let sqlite = crate::db::sqlite::init_sqlite(&data_dir.join("search.db"))
            .expect("SQLite FTS5 init failed");
        let (event_tx, _) = tokio::sync::broadcast::channel(64);
        let (ws_shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        Self {
            sqlite,
            tunnel_url: Arc::new(RwLock::new(None)),
            event_tx,
            ws_shutdown_tx,
            agent_sessions: Arc::new(Mutex::new(HashMap::new())),
            intent_centroids: Arc::new(Mutex::new(None)),
            vault_path_cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
            http_client: reqwest::Client::new(),
            llm_url: config.llm_url(),
            embedding_url: config.embedding_url(),
            whisper_url: config.whisper_url(),
            whisper_server: Arc::new(Mutex::new(None)),
            whisper_start_lock: Arc::new(Mutex::new(())),
            whisper_user_stopped: Arc::new(AtomicBool::new(false)),
            llama_server: Arc::new(Mutex::new(None)),
            llama_start_lock: Arc::new(Mutex::new(())),
            llama_user_stopped: Arc::new(AtomicBool::new(false)),
            embedding_server: Arc::new(Mutex::new(None)),
            embedding_start_lock: Arc::new(Mutex::new(())),
            embedding_user_stopped: Arc::new(AtomicBool::new(false)),
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
