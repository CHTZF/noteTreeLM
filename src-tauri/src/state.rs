use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::db::surreal::SurrealDb;
use crate::error::AppError;

#[derive(Clone)]
pub struct AppState {
    /// 單一 SurrealDB 實例（embedded SurrealKV），取代原本的 settings_db + vault_db
    pub db: SurrealDb,
    /// 目前 Vault 的路徑（同時作為 vault_id 使用）
    pub vault_path: Arc<RwLock<String>>,
    /// 持有 llama-server 子進程；App 結束時 kill
    pub llama_server: Arc<Mutex<Option<tokio::process::Child>>>,
    /// 持有 whisper-server 子進程；App 結束時 kill
    pub whisper_server: Arc<Mutex<Option<tokio::process::Child>>>,
    /// FileWatcher 停止信號（drop sender 即可停止舊 watcher thread）
    pub watcher_stop: Arc<Mutex<Option<std::sync::mpsc::SyncSender<()>>>>,
    /// 寫入工具確認通道（stream_chat 等待前端確認時使用）
    pub write_confirm_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
    /// llama-server 實際執行的 port（執行時自動分配，不存 DB）
    pub llama_actual_port: Arc<Mutex<Option<u16>>>,
    /// whisper-server 實際執行的 port（執行時自動分配，不存 DB）
    pub whisper_actual_port: Arc<Mutex<Option<u16>>>,
    /// llama-server 啟動鎖：防止多個呼叫者同時跑啟動流程，導致重複 emit 就緒訊息
    pub llama_start_lock: Arc<Mutex<()>>,
    /// whisper-server 啟動鎖：同上
    pub whisper_start_lock: Arc<Mutex<()>>,
    /// 使用者主動停止旗標：true 時 ensure_whisper_server_running 不會自動重啟
    pub whisper_user_stopped: Arc<AtomicBool>,
    /// 使用者主動停止旗標：true 時 ensure_server_running 不會自動重啟
    pub llama_user_stopped: Arc<AtomicBool>,
    /// 持有 embedding-server 子進程；App 結束時 kill
    pub embedding_server: Arc<Mutex<Option<tokio::process::Child>>>,
    /// embedding-server 實際執行的 port
    pub embedding_actual_port: Arc<Mutex<Option<u16>>>,
    /// embedding-server 啟動鎖
    pub embedding_start_lock: Arc<Mutex<()>>,
    /// 使用者主動停止旗標：true 時 ensure_embedding_server_running 不會自動重啟
    pub embedding_user_stopped: Arc<AtomicBool>,
    /// 全域 port 分配鎖：防止 whisper 與 llama 並發 find_free_port 時取到同一個 port
    pub port_allocator: Arc<Mutex<()>>,
    /// Agent 取消旗標：設為 true 時 invoke_agent 的 SSE 迴圈中止
    pub agent_cancel: Arc<AtomicBool>,
    /// Agent 目前活躍的 session_id（None 表示閒置）
    pub agent_session: Arc<Mutex<Option<String>>>,
    /// 工具測試台取消旗標：設為 true 時 run_tool_pipeline 的步驟迴圈中止
    pub tool_test_cancel: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(db: SurrealDb) -> Self {
        Self {
            db,
            vault_path: Arc::new(RwLock::new(String::new())),
            llama_server: Arc::new(Mutex::new(None)),
            whisper_server: Arc::new(Mutex::new(None)),
            watcher_stop: Arc::new(Mutex::new(None)),
            write_confirm_tx: Arc::new(Mutex::new(None)),
            llama_actual_port: Arc::new(Mutex::new(None)),
            whisper_actual_port: Arc::new(Mutex::new(None)),
            llama_start_lock: Arc::new(Mutex::new(())),
            whisper_start_lock: Arc::new(Mutex::new(())),
            whisper_user_stopped: Arc::new(AtomicBool::new(false)),
            llama_user_stopped: Arc::new(AtomicBool::new(false)),
            embedding_server: Arc::new(Mutex::new(None)),
            embedding_actual_port: Arc::new(Mutex::new(None)),
            embedding_start_lock: Arc::new(Mutex::new(())),
            embedding_user_stopped: Arc::new(AtomicBool::new(false)),
            port_allocator: Arc::new(Mutex::new(())),
            agent_cancel: Arc::new(AtomicBool::new(false)),
            agent_session: Arc::new(Mutex::new(None)),
            tool_test_cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn get_vault_path(&self) -> String {
        self.vault_path.read().await.clone()
    }

    pub async fn set_vault_path(&self, path: String) {
        *self.vault_path.write().await = path;
    }

    /// 取得目前 vault 的 ID（使用 vault_path）；Vault 未設定時回傳錯誤
    pub async fn get_vault_id(&self) -> Result<String, AppError> {
        let path = self.vault_path.read().await.clone();
        if path.is_empty() {
            Err(AppError::Vault("尚未設定 Vault 路徑".to_string()))
        } else {
            Ok(path)
        }
    }
}
