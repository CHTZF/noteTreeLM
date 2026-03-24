use std::sync::Arc;
use tokio::sync::RwLock;
use serde::{Deserialize, Serialize};

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
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            servers: Arc::new(RwLock::new(Vec::new())),
        }
    }
}
