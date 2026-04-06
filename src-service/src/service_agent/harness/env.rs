use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::Mutex;

use crate::api_state::ApiState;
use crate::db::SurrealDb;
use crate::service_agent::engine::transaction::AnswerChannel;
use super::memory::working::WorkingMemory;

/// Captures all per-session context needed to execute any interactive tool.
/// Passed as `Arc<VaultEnv>` into each tool handler, replacing the ~10 individual
/// variables previously cloned into every closure inside build_interactive_registry.
pub(crate) struct VaultEnv {
    pub client:         reqwest::Client,
    pub llm_url:        String,
    pub db:             SurrealDb,
    pub vault_id:       String,
    pub account_id:     String,
    pub vault_path:     String,
    pub embedding_url:  Option<String>,
    pub session_id:     String,
    pub conv_id:        String,
    pub state:          ApiState,
    pub cancel:         Arc<AtomicBool>,
    pub answer_channel: Arc<AnswerChannel>,
    /// Per-session tool execution evidence store.
    /// Write via `working_memory.record()`; read via `working_memory.with_records()`.
    pub working_memory: WorkingMemory,
    /// Pre-write file snapshots keyed by vault-relative path (with .md suffix).
    /// Written by write-tool forward handlers; read by rollback handlers.
    pub write_snapshots: Arc<Mutex<HashMap<String, String>>>,
    /// Modification times (mtime, secs since epoch) recorded when snapshots were taken.
    /// Used by write conflict detection: if mtime has advanced since we read, someone else wrote.
    pub write_mtimes: Arc<Mutex<HashMap<String, u64>>>,
}
