use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, oneshot};

/// Transaction 職責邊界：
/// - 狀態機（Idle ↔ Preparing → Cancelled）驅動寫入確認流程
/// - `prepare()` — Executor 遇到寫入工具時呼叫，設 Preparing、回傳 Receiver 等待確認
/// - `commit()` — 使用者核准 → state 回到 Idle（允許同一 round 多個寫入工具逐一確認）
/// - `cancel()` — 使用者拒絕或 cancel_agent → Cancelled，Executor 停止所有後續執行
/// - `record_tool()` — 每次工具執行後由 Executor 呼叫（audit log）
#[derive(Clone, PartialEq, Debug)]
pub enum TransactionState {
    Idle,
    Preparing,
    Cancelled,
}

pub struct Transaction {
    state: Mutex<TransactionState>,
    /// 此 transaction 中被執行的工具名稱（按執行順序）
    tools: Mutex<Vec<String>>,
    /// 寫入確認 oneshot sender（由 prepare 建立、commit/cancel 消費）
    confirm_tx: Mutex<Option<oneshot::Sender<bool>>>,
    /// pending plan 確認旗標：true 時 prepare() 跳過等待直接核准，避免使用者二次確認
    pre_approved: AtomicBool,
}

impl Transaction {

    pub fn new() -> Self {
        Self {
            state: Mutex::new(TransactionState::Idle),
            tools: Mutex::new(Vec::new()),
            confirm_tx: Mutex::new(None),
            pre_approved: AtomicBool::new(false),
        }
    }

    /// Pending plan 確認後呼叫：標記本輪所有寫入工具無需再次確認。
    pub fn pre_approve(&self) {
        self.pre_approved.store(true, Ordering::Relaxed);
    }

    // ── 寫入確認協議（由 Executor + 前端 confirm route 驅動）─────────────────

    /// Executor 在執行寫入工具前呼叫。
    /// - 若 pre_approved → 直接回傳 ready receiver（true），跳過等待
    /// - 若已是 Cancelled → 回傳 Err（Executor 應立即停止）
    /// - 否則 → state = Preparing，建立 oneshot channel 供 Executor await
    pub async fn prepare(&self) -> Result<oneshot::Receiver<bool>, String> {
        if self.pre_approved.load(Ordering::Relaxed) {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(true);
            return Ok(rx);
        }
        let mut state = self.state.lock().await;
        if *state == TransactionState::Cancelled {
            return Err("Transaction is cancelled".into());
        }
        *state = TransactionState::Preparing;
        let (tx, rx) = oneshot::channel();
        *self.confirm_tx.lock().await = Some(tx);
        Ok(rx)
    }

    /// 使用者核准寫入（POST /agent/confirm approved=true）。
    /// 喚醒 Executor（send true）、state 回到 Idle 以允許下一個寫入工具確認。
    pub async fn commit(&self) -> Result<(), String> {
        {
            let mut state = self.state.lock().await;
            *state = TransactionState::Idle;
        }
        if let Some(tx) = self.confirm_tx.lock().await.take() {
            let _ = tx.send(true);
        }
        Ok(())
    }

    /// 使用者拒絕寫入或 cancel_agent 呼叫。
    /// 喚醒 Executor（send false）、state 設為 Cancelled（阻止後續所有執行）。
    pub async fn cancel(&self) -> Result<(), String> {
        {
            let mut state = self.state.lock().await;
            *state = TransactionState::Cancelled;
        }
        if let Some(tx) = self.confirm_tx.lock().await.take() {
            let _ = tx.send(false);
        }
        Ok(())
    }

    pub async fn state(&self) -> TransactionState {
        self.state.lock().await.clone()
    }

    // ── 工具記錄（由 Executor 在每次工具執行後呼叫）──────────────────────────

    pub async fn record_tool(&self, name: &str) {
        self.tools.lock().await.push(name.to_string());
    }

    // ── 讀取（由 Agent 在 emit debug 事件時使用）─────────────────────────────

    pub async fn get_tools(&self) -> Vec<String> {
        self.tools.lock().await.clone()
    }
}

/// One-shot channel for the `ask_user` tool / Step-0b resume flow.
///
/// Shared as `Arc<AnswerChannel>` between `AgentSession` (for Step-0b lookup)
/// and the running tool handler (which calls `wait()`).
/// Using interior mutability means callers only need `&self` — no `&mut AgentSession`.
pub struct AnswerChannel {
    tx: Mutex<Option<oneshot::Sender<String>>>,
}

impl AnswerChannel {
    pub fn new() -> Self {
        Self { tx: Mutex::new(None) }
    }

    /// Called by `ask_user` tool: registers a sender and returns the receiver to await on.
    pub async fn wait(&self) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        *self.tx.lock().await = Some(tx);
        rx
    }

    /// Called by Step-0b: if a sender is waiting, forward `answer` and return `true`.
    /// Returns `false` if no sender is registered (normal new-turn case).
    pub async fn try_resume(&self, answer: String) -> bool {
        if let Some(tx) = self.tx.lock().await.take() {
            let _ = tx.send(answer);
            true
        } else {
            false
        }
    }
}
