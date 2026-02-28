use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub vault_path: Arc<RwLock<String>>,
    /// 持有 llama-server 子進程；App 結束時 kill
    pub llama_server: Arc<Mutex<Option<tokio::process::Child>>>,
    /// 持有 whisper-server 子進程；App 結束時 kill
    pub whisper_server: Arc<Mutex<Option<tokio::process::Child>>>,
}

impl AppState {
    pub fn new(db: SqlitePool) -> Self {
        Self {
            db,
            vault_path: Arc::new(RwLock::new(String::new())),
            llama_server: Arc::new(Mutex::new(None)),
            whisper_server: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn get_vault_path(&self) -> String {
        self.vault_path.read().await.clone()
    }

    pub async fn set_vault_path(&self, path: String) {
        *self.vault_path.write().await = path;
    }
}
