use tokio::sync::Mutex;

/// Transaction 職責邊界：
/// - 狀態機（Idle → Preparing → Committed / Cancelled）由 Agent 驅動
/// - 工具記錄（record_tool）由 Executor 在每次工具執行後呼叫
/// - 讀取（get_tools / state）由 Agent 在 emit debug 事件時使用
#[derive(Clone, PartialEq, Debug)]
pub enum TransactionState {
    Idle,
    Preparing,
    Committed,
    Cancelled,
}

pub struct Transaction {
    state: Mutex<TransactionState>,
    /// 此 transaction 中被執行的工具名稱（按執行順序）
    tools: Mutex<Vec<String>>,
}

impl Transaction {

    pub fn new() -> Self {
        Self {
            state: Mutex::new(TransactionState::Idle),
            tools: Mutex::new(Vec::new()),
        }
    }

    // ── 狀態機（由 Agent 呼叫）────────────────────────────────────

    pub async fn prepare(&self) -> Result<(), String> {

        let mut state = self.state.lock().await;

        if *state != TransactionState::Idle {
            return Err("Transaction not Idle".into());
        }

        *state = TransactionState::Preparing;

        Ok(())
    }

    pub async fn commit(&self) -> Result<(), String> {

        let mut state = self.state.lock().await;

        if *state != TransactionState::Preparing {
            return Err("Transaction not prepared".into());
        }

        *state = TransactionState::Committed;

        Ok(())
    }

    pub async fn cancel(&self) -> Result<(), String> {

        let mut state = self.state.lock().await;

        *state = TransactionState::Cancelled;

        Ok(())
    }

    pub async fn state(&self) -> TransactionState {

        let state = self.state.lock().await;

        state.clone()
    }

    // ── 工具記錄（由 Executor 呼叫）──────────────────────────────

    pub async fn record_tool(&self, name: &str) {
        self.tools.lock().await.push(name.to_string());
    }

    // ── 讀取（由 Agent 在 emit 時使用）───────────────────────────

    pub async fn get_tools(&self) -> Vec<String> {
        self.tools.lock().await.clone()
    }
}
