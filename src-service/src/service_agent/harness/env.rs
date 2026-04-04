use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::api_state::ApiState;
use crate::db::SurrealDb;
use crate::service_agent::engine::dispatcher::ToolCallStore;

/// Captures all per-session context needed to execute any interactive tool.
/// Passed as `Arc<VaultEnv>` into each tool handler, replacing the ~10 individual
/// variables previously cloned into every closure inside build_interactive_registry.
pub(crate) struct VaultEnv {
    pub client:        reqwest::Client,
    pub llm_url:       String,
    pub db:            SurrealDb,
    pub vault_id:      String,
    pub account_id:    String,
    pub vault_path:    String,
    pub embedding_url: Option<String>,
    pub session_id:    String,
    pub state:         ApiState,
    pub cancel:        Arc<AtomicBool>,
    pub tool_calls_store: ToolCallStore,
}
