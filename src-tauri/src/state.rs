use sqlx::SqlitePool;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::error::AppError;

#[derive(Clone)]
pub struct AppState {
    /// 帳號層級設定 DB（app_data_dir/settings.db）
    pub settings_db: SqlitePool,
    /// 目前 Vault 的資料 DB（vault_path/.notetreelm.db）；未設定 vault 時為 None
    vault_db: Arc<RwLock<Option<SqlitePool>>>,
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
    /// 全域 port 分配鎖：防止 whisper 與 llama 並發 find_free_port 時取到同一個 port
    pub port_allocator: Arc<Mutex<()>>,
}

impl AppState {
    pub fn new(settings_db: SqlitePool) -> Self {
        Self {
            settings_db,
            vault_db: Arc::new(RwLock::new(None)),
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
            port_allocator: Arc::new(Mutex::new(())),
        }
    }

    pub async fn get_vault_path(&self) -> String {
        self.vault_path.read().await.clone()
    }

    pub async fn set_vault_path(&self, path: String) {
        *self.vault_path.write().await = path;
    }

    /// 取得 vault DB pool；Vault 未設定時回傳錯誤
    pub async fn get_vault_db(&self) -> Result<SqlitePool, AppError> {
        self.vault_db
            .read()
            .await
            .clone()
            .ok_or_else(|| AppError::Vault("尚未設定 Vault 路徑".to_string()))
    }

    /// 設定（或清除）vault DB pool
    pub async fn set_vault_db(&self, pool: Option<SqlitePool>) {
        *self.vault_db.write().await = pool;
    }
}
