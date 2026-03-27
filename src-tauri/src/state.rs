use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use reqwest;

use crate::error::AppError;
use crate::runtime::system_agent::SystemAgentService;

#[derive(Clone)]
pub struct AppState {
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
    /// System Agent Service — rule-based broker，負責路由 call_agent 請求
    pub system_agent: Arc<SystemAgentService>,
    /// API key 記憶體快取：provider → key，避免重複呼叫 keychain
    pub api_key_cache: Arc<Mutex<HashMap<String, String>>>,
    /// 共用 HTTP client（所有 LLM / embedding 呼叫共用，避免重複建立 connection pool）
    pub http_client: reqwest::Client,
    /// Intent centroid 快取（confirm / cancel / interrupt 三組固定詞組的 embedding 平均值）
    /// 首次計算後永久快取；詞組不變，無需失效機制
    pub intent_centroids: Arc<Mutex<Option<(Vec<f32>, Vec<f32>, Vec<f32>)>>>,
    /// 已設置標題的對話 ID 集合（in-memory）：防止 maybe_set_title 每輪重複 DB query
    pub titled_convs: Arc<Mutex<HashSet<String>>>,
    /// Live chat 執行旗標：true 時 make_confirm_write 自動核准寫入（無需前端確認）
    pub live_chat_active: Arc<AtomicBool>,
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
        let http_client = reqwest::Client::new();
        let auth_token: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
        let system_agent = Arc::new(SystemAgentService::new(http_client.clone(), auth_token.clone()));
        Self {
            system_agent,
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
            api_key_cache: Arc::new(Mutex::new(HashMap::new())),
            http_client,
            intent_centroids: Arc::new(Mutex::new(None)),
            titled_convs: Arc::new(Mutex::new(HashSet::new())),
            live_chat_active: Arc::new(AtomicBool::new(false)),
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

    /// vault_id 設定後同步更新 SystemAgentService
    pub async fn set_vault_path_with_agent(&self, path: String) {
        self.system_agent.set_vault_id(path.clone()).await;
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
