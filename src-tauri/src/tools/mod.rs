/// tools/mod.rs
/// 將 vault 工具包裝成 runtime ToolRegistry 格式。
///
/// 子模組：
/// - `readonly`  — 唯讀工具（list_structure / read_note / search_vault / query_memory 等）
/// - `write`     — 寫入工具（含 rollback）及副作用工具
/// - `agent`     — Agent 路由工具（web_search / create_agent_skill / list_available_agents 等）
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tauri::AppHandle;

use crate::runtime::tool_registry::ToolRegistry;

mod agent;
mod readonly;
mod write;

/// 建立工具所需的共用上下文（build_vault_registry 的所有參數打包為一個結構體）
pub(crate) struct BuildCtx {
    pub vault_path: String,
    pub vault_id: String,
    pub http_client: reqwest::Client,
    pub auth_token: String,
    pub app: AppHandle,
    pub emb_url: Option<String>,
    pub cancel: Option<Arc<AtomicBool>>,
}

/// 建立包含所有 vault 工具的 ToolRegistry。
pub fn build_vault_registry(
    vault_path: String,
    vault_id: String,
    http_client: reqwest::Client,
    auth_token: String,
    app: AppHandle,
    emb_url: Option<String>,
    cancel: Option<Arc<AtomicBool>>,
) -> Arc<ToolRegistry> {
    let ctx = BuildCtx {
        vault_path,
        vault_id,
        http_client,
        auth_token,
        app,
        emb_url,
        cancel,
    };

    let mut registry = ToolRegistry::new();
    readonly::register(&mut registry, &ctx);
    write::register(&mut registry, &ctx);
    agent::register(&mut registry, &ctx);
    Arc::new(registry)
}

