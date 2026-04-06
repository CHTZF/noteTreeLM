use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::service::types::{AgentSession, ServiceEvent};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub port: u16,
    pub status: String,   // "running" | "stopped" | "error"
    pub model: Option<String>,
    pub updated_at: i64,
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
