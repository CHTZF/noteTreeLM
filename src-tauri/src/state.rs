use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use reqwest;

use crate::error::AppError;

#[derive(Clone)]
pub struct AppState {
    /// 目前 Vault 的路徑（同時作為 vault_id 使用）
    pub vault_path: Arc<RwLock<String>>,
    /// FileWatcher 停止信號（drop sender 即可停止舊 watcher thread）
    pub watcher_stop: Arc<Mutex<Option<std::sync::mpsc::SyncSender<()>>>>,
    /// 寫入工具確認通道（stream_chat 等待前端確認時使用）
    pub write_confirm_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
    /// Agent 取消旗標：設為 true 時 invoke_agent 的 SSE 迴圈中止
    pub agent_cancel: Arc<AtomicBool>,
    /// Agent 目前活躍的 session_id（None 表示閒置）
    pub agent_session: Arc<Mutex<Option<String>>>,
    /// 工具測試台取消旗標：設為 true 時 run_tool_pipeline 的步驟迴圈中止
    pub tool_test_cancel: Arc<AtomicBool>,
    /// API key 記憶體快取：provider → key，避免重複呼叫 keychain
    pub api_key_cache: Arc<Mutex<HashMap<String, String>>>,
    /// 共用 HTTP client（所有 LLM / embedding 呼叫共用，避免重複建立 connection pool）
    pub http_client: reqwest::Client,
    /// 目前 Vault 的 UUID（DB 查詢使用，與 vault_path 分離）
    pub vault_uuid: Arc<RwLock<String>>,
    /// 目前登入的使用者名稱
    pub username: Arc<RwLock<String>>,
    /// DB/init 就緒旗標：背景 init task 完成後設為 true，通知前端可進入 App
    pub app_ready: Arc<std::sync::atomic::AtomicBool>,
    /// 目前登入的 daemon auth token（Bearer token for API calls）
    pub auth_token: Arc<RwLock<String>>,
}

impl AppState {
    pub fn new() -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        let auth_token: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
        Self {
            vault_path: Arc::new(RwLock::new(String::new())),
            watcher_stop: Arc::new(Mutex::new(None)),
            write_confirm_tx: Arc::new(Mutex::new(None)),
            agent_cancel: Arc::new(AtomicBool::new(false)),
            agent_session: Arc::new(Mutex::new(None)),
            tool_test_cancel: Arc::new(AtomicBool::new(false)),
            api_key_cache: Arc::new(Mutex::new(HashMap::new())),
            http_client,
            vault_uuid: Arc::new(RwLock::new(String::new())),
            username: Arc::new(RwLock::new(String::new())),
            app_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            auth_token,
        }
    }

    pub async fn get_auth_token(&self) -> String {
        self.auth_token.read().await.clone()
    }

    pub async fn set_auth_token(&self, token: String) {
        *self.auth_token.write().await = token;
    }

    pub async fn clear_auth_token(&self) {
        *self.auth_token.write().await = String::new();
    }

    pub async fn set_vault_path_with_agent(&self, path: String) {
        *self.vault_path.write().await = path;
    }

    pub async fn get_vault_path(&self) -> String {
        self.vault_path.read().await.clone()
    }

    pub async fn set_vault_path(&self, path: String) {
        *self.vault_path.write().await = path;
    }

    /// 取得目前 vault 的 UUID；Vault 未設定時回傳錯誤
    pub async fn get_vault_id(&self) -> Result<String, AppError> {
        let uuid = self.vault_uuid.read().await.clone();
        if uuid.is_empty() {
            Err(AppError::Vault("尚未設定 Vault".to_string()))
        } else {
            Ok(uuid)
        }
    }

    pub async fn get_vault_uuid(&self) -> String {
        self.vault_uuid.read().await.clone()
    }

    pub async fn set_vault_uuid(&self, uuid: String) {
        *self.vault_uuid.write().await = uuid;
    }

    pub async fn get_username(&self) -> String {
        self.username.read().await.clone()
    }

    pub async fn set_username(&self, name: String) {
        *self.username.write().await = name;
    }
}
