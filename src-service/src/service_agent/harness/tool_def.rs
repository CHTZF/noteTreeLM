use std::sync::Arc;

use serde_json::{json, Value};

use super::env::VaultEnv;
use crate::service_agent::types::ToolFuture;

// ── Guard types (co-located with tool definitions) ────────────────────────────

/// Evidence tier required before a guarded write tool may execute.
#[derive(Copy, Clone)]
pub(crate) enum GuardLevel {
    /// Path must appear in a prior tool's result (search/list result array, or read/open args).
    PathSeen,
    /// Additionally, read_note must have returned non-error content for this exact path.
    ContentRead,
}

/// Declarative precondition spec. All fields are fn pointers → ToolDef stays Copy.
#[derive(Copy, Clone)]
pub(crate) struct ToolGuardSpec {
    /// Extract the target path string from the tool's args.
    pub path_extractor: fn(&Value) -> String,
    /// Minimum evidence level required.
    pub require: GuardLevel,
    /// If true, target is a folder (skip .md normalisation; match "folder/" in list_structure text).
    pub is_folder: bool,
}

// ── ToolDef ───────────────────────────────────────────────────────────────────

/// Synchronous handler function pointer: takes the shared env + args, returns a boxed future.
/// Using fn pointer (not Box<dyn Fn>) keeps ToolDef Copy and zero-allocation.
pub(crate) type HandlerFn = fn(Arc<VaultEnv>, Value) -> ToolFuture;

/// A single tool's complete definition: schema + guard + write-flag + handler, all co-located.
/// Adding a new tool means adding ONE entry here — no other files need touching.
#[derive(Copy, Clone)]
pub(crate) struct ToolDef {
    pub name:      &'static str,
    /// Returns the OpenAI-compatible function schema for this tool.
    pub schema_fn: fn() -> Value,
    /// Whether this tool modifies vault/DB state and requires user confirmation.
    pub is_write:  bool,
    /// If Some, the guard is evaluated before executing the handler.
    pub guard:     Option<ToolGuardSpec>,
    pub handler:   HandlerFn,
}

// ── Static tool registry ──────────────────────────────────────────────────────

/// The canonical list of all interactive tools.
/// Schema, guard spec, write flag, and handler are defined together for each tool.
pub(crate) static ALL_TOOL_DEFS: &[ToolDef] = &[
    // ── Think (forced Round-0 reasoning; never in tool_names) ────────────────
    ToolDef { name: "think",          schema_fn: schema_think,          is_write: false, guard: None, handler: handle_think },

    // ── Vault read tools ─────────────────────────────────────────────────────
    ToolDef { name: "list_structure", schema_fn: schema_list_structure, is_write: false, guard: None, handler: handle_list_structure },
    ToolDef { name: "read_note",      schema_fn: schema_read_note,      is_write: false, guard: None, handler: handle_read_note },
    ToolDef { name: "search_vault",   schema_fn: schema_search_vault,   is_write: false, guard: None, handler: handle_search_vault },
    ToolDef { name: "query_memory",   schema_fn: schema_query_memory,   is_write: false, guard: None, handler: handle_query_memory },

    // ── Vault write tools ────────────────────────────────────────────────────
    ToolDef { name: "create_note",    schema_fn: schema_create_note,    is_write: true,  guard: None,                  handler: handle_create_note },
    ToolDef { name: "create_folder",  schema_fn: schema_create_folder,  is_write: true,  guard: None,                  handler: handle_create_folder },
    ToolDef { name: "update_note",    schema_fn: schema_update_note,    is_write: true,  guard: Some(GUARD_UPDATE_NOTE),    handler: handle_update_note },
    ToolDef { name: "append_to_note", schema_fn: schema_append_to_note, is_write: true,  guard: Some(GUARD_APPEND_TO_NOTE), handler: handle_append_to_note },
    ToolDef { name: "delete_note",    schema_fn: schema_delete_note,    is_write: true,  guard: Some(GUARD_DELETE_NOTE),    handler: handle_delete_note },
    ToolDef { name: "delete_folder",  schema_fn: schema_delete_folder,  is_write: true,  guard: Some(GUARD_DELETE_FOLDER),  handler: handle_delete_folder },
    ToolDef { name: "move_note",      schema_fn: schema_move_note,      is_write: true,  guard: Some(GUARD_MOVE_NOTE),      handler: handle_move_note },

    // ── Agent / UI tools ─────────────────────────────────────────────────────
    ToolDef { name: "plan_announce",       schema_fn: schema_plan_announce,       is_write: false, guard: None, handler: handle_plan_announce },
    ToolDef { name: "open_note",           schema_fn: schema_open_note,           is_write: false, guard: None, handler: handle_open_note },
    ToolDef { name: "create_agent_skill",  schema_fn: schema_create_agent_skill,  is_write: true,  guard: None, handler: handle_create_agent_skill },
    ToolDef { name: "call_agent",          schema_fn: schema_call_agent,          is_write: false, guard: None, handler: handle_call_agent },
    ToolDef { name: "live_respond",        schema_fn: schema_live_respond,        is_write: false, guard: None, handler: handle_live_respond },

    // ── Memory agent tools ───────────────────────────────────────────────────
    ToolDef { name: "get_unprocessed_conversations", schema_fn: schema_get_unprocessed_conversations, is_write: false, guard: None, handler: handle_get_unprocessed_conversations },
    ToolDef { name: "get_conversation_content",      schema_fn: schema_get_conversation_content,      is_write: false, guard: None, handler: handle_get_conversation_content },
    ToolDef { name: "save_memory_facts",             schema_fn: schema_save_memory_facts,             is_write: true,  guard: None, handler: handle_save_memory_facts },
    ToolDef { name: "mark_conversation_processed",   schema_fn: schema_mark_conversation_processed,   is_write: true,  guard: None, handler: handle_mark_conversation_processed },
    ToolDef { name: "condense_memory_facts",         schema_fn: schema_condense_memory_facts,         is_write: true,  guard: None, handler: handle_condense_memory_facts },
];

/// Convenience: look up a ToolDef by name.
pub(crate) fn find_tool_def(name: &str) -> Option<&'static ToolDef> {
    ALL_TOOL_DEFS.iter().find(|d| d.name == name)
}

/// Build the OpenAI tool schema list for the given tool names.
/// Replaces build_tools_schema_interactive in vault_tools.rs.
pub(crate) fn build_tools_schema(tool_names: &[String]) -> Vec<Value> {
    ALL_TOOL_DEFS.iter()
        .filter(|d| tool_names.iter().any(|n| n == d.name))
        .map(|d| (d.schema_fn)())
        .collect()
}

// ── Guard specs ───────────────────────────────────────────────────────────────

const GUARD_UPDATE_NOTE: ToolGuardSpec = ToolGuardSpec {
    path_extractor: |args| args["path"].as_str().unwrap_or("").to_string(),
    require:        GuardLevel::ContentRead,
    is_folder:      false,
};
const GUARD_APPEND_TO_NOTE: ToolGuardSpec = ToolGuardSpec {
    path_extractor: |args| args["path"].as_str().unwrap_or("").to_string(),
    require:        GuardLevel::ContentRead,
    is_folder:      false,
};
const GUARD_DELETE_NOTE: ToolGuardSpec = ToolGuardSpec {
    path_extractor: |args| args["path"].as_str().unwrap_or("").to_string(),
    require:        GuardLevel::PathSeen,
    is_folder:      false,
};
const GUARD_DELETE_FOLDER: ToolGuardSpec = ToolGuardSpec {
    path_extractor: |args| args["path"].as_str().unwrap_or("").to_string(),
    require:        GuardLevel::PathSeen,
    is_folder:      true,
};
const GUARD_MOVE_NOTE: ToolGuardSpec = ToolGuardSpec {
    path_extractor: |args| args["from"].as_str().unwrap_or("").to_string(),
    require:        GuardLevel::PathSeen,
    is_folder:      false,
};

// ── Schema functions (one per tool) ──────────────────────────────────────────

fn schema_think() -> Value { json!({ "type": "function", "function": {
    "name": "think",
    "description": "在回應前先進行推理思考（不直接輸出給使用者）",
    "parameters": { "type": "object", "properties": {
        "thought": { "type": "string", "description": "推理過程" }
    }, "required": ["thought"] }
}})}

fn schema_list_structure() -> Value { json!({ "type": "function", "function": {
    "name": "list_structure",
    "description": "列出 vault 的資料夾和筆記結構",
    "parameters": { "type": "object", "properties": {
        "path": { "type": "string", "description": "子路徑，省略則顯示根目錄" }
    }, "required": [] }
}})}

fn schema_read_note() -> Value { json!({ "type": "function", "function": {
    "name": "read_note",
    "description": "讀取指定路徑的筆記內容",
    "parameters": { "type": "object", "properties": {
        "path": { "type": "string", "description": "筆記的相對路徑（可省略 .md）" }
    }, "required": ["path"] }
}})}

fn schema_search_vault() -> Value { json!({ "type": "function", "function": {
    "name": "search_vault",
    "description": "在 vault 中搜尋相關筆記",
    "parameters": { "type": "object", "properties": {
        "query": { "type": "string", "description": "搜尋關鍵字" }
    }, "required": ["query"] }
}})}

fn schema_query_memory() -> Value { json!({ "type": "function", "function": {
    "name": "query_memory",
    "description": "查詢長期記憶事實",
    "parameters": { "type": "object", "properties": {
        "keywords": { "type": "array", "items": { "type": "string" }, "description": "關鍵字列表" },
        "limit":    { "type": "number", "description": "最多幾條，預設 5" }
    }, "required": [] }
}})}

fn schema_create_note() -> Value { json!({ "type": "function", "function": {
    "name": "create_note",
    "description": "建立新筆記",
    "parameters": { "type": "object", "properties": {
        "path":    { "type": "string", "description": "筆記路徑（可省略 .md）" },
        "content": { "type": "string", "description": "筆記內容（Markdown）" }
    }, "required": ["path", "content"] }
}})}

fn schema_update_note() -> Value { json!({ "type": "function", "function": {
    "name": "update_note",
    "description": "更新現有筆記的全部內容",
    "parameters": { "type": "object", "properties": {
        "path":    { "type": "string", "description": "筆記路徑（可省略 .md）" },
        "content": { "type": "string", "description": "新的筆記內容" }
    }, "required": ["path", "content"] }
}})}

fn schema_append_to_note() -> Value { json!({ "type": "function", "function": {
    "name": "append_to_note",
    "description": "在現有筆記末尾追加內容",
    "parameters": { "type": "object", "properties": {
        "path":    { "type": "string", "description": "筆記路徑（可省略 .md）" },
        "content": { "type": "string", "description": "要追加的內容" }
    }, "required": ["path", "content"] }
}})}

fn schema_create_folder() -> Value { json!({ "type": "function", "function": {
    "name": "create_folder",
    "description": "建立新資料夾",
    "parameters": { "type": "object", "properties": {
        "path": { "type": "string", "description": "資料夾相對路徑" }
    }, "required": ["path"] }
}})}

fn schema_delete_note() -> Value { json!({ "type": "function", "function": {
    "name": "delete_note",
    "description": "刪除指定的筆記（不可恢復）",
    "parameters": { "type": "object", "properties": {
        "path": { "type": "string", "description": "筆記路徑（可省略 .md）" }
    }, "required": ["path"] }
}})}

fn schema_delete_folder() -> Value { json!({ "type": "function", "function": {
    "name": "delete_folder",
    "description": "刪除整個資料夾及其內容（不可恢復）",
    "parameters": { "type": "object", "properties": {
        "path": { "type": "string", "description": "資料夾路徑" }
    }, "required": ["path"] }
}})}

fn schema_move_note() -> Value { json!({ "type": "function", "function": {
    "name": "move_note",
    "description": "移動或重新命名筆記",
    "parameters": { "type": "object", "properties": {
        "from": { "type": "string", "description": "來源路徑（可省略 .md）" },
        "to":   { "type": "string", "description": "目標路徑（可省略 .md）" }
    }, "required": ["from", "to"] }
}})}

fn schema_plan_announce() -> Value { json!({ "type": "function", "function": {
    "name": "plan_announce",
    "description": "在執行寫入操作前，向使用者宣告計畫。呼叫後自動繼續執行，不需確認。",
    "parameters": { "type": "object", "properties": {
        "plan": { "type": "string", "description": "即將執行的操作計畫描述" }
    }, "required": ["plan"] }
}})}

fn schema_open_note() -> Value { json!({ "type": "function", "function": {
    "name": "open_note",
    "description": "在編輯器中打開指定筆記，讓使用者查看。呼叫後對話結束。",
    "parameters": { "type": "object", "properties": {
        "paths": { "type": "array", "items": { "type": "string" }, "description": "要打開的筆記路徑列表" }
    }, "required": ["paths"] }
}})}

fn schema_create_agent_skill() -> Value { json!({ "type": "function", "function": {
    "name": "create_agent_skill",
    "description": "建立新的 agent 技能。behavior 欄位用自然語言描述，工具鏈用 @[tool_name] 標記。",
    "parameters": { "type": "object", "properties": {
        "title":          { "type": "string", "description": "技能名稱" },
        "trigger":        { "type": "string", "description": "觸發關鍵詞，多個以逗號分隔" },
        "behavior":       { "type": "string", "description": "行為描述；工具鏈以 @[tool_name] 標記順序" },
        "injection_mode": { "type": "string", "description": "passive / active / proactive" }
    }, "required": ["title", "trigger", "behavior"] }
}})}

fn schema_call_agent() -> Value { json!({ "type": "function", "function": {
    "name": "call_agent",
    "description": "呼叫另一個已定義的 agent 執行特定任務，並取回結果",
    "parameters": { "type": "object", "properties": {
        "name":  { "type": "string", "description": "agent 定義的名稱" },
        "input": { "type": "string", "description": "傳給 sub-agent 的任務描述或問題" }
    }, "required": ["name", "input"] }
}})}

fn schema_live_respond() -> Value { json!({ "type": "function", "function": {
    "name": "live_respond",
    "description": "輸出語音助理的最終口語回覆（live chat 專用，呼叫後對話結束）",
    "parameters": { "type": "object", "properties": {
        "speech":  { "type": "string", "description": "TTS 朗讀文字，口語化，2 句以內，不含 Markdown" },
        "action":  { "type": "string", "description": "show_results / open_note / open_tab / show_error / none" },
        "content": { "type": "string", "description": "若有查到資料，把完整摘要放此供畫面顯示（可含換行）" }
    }, "required": ["speech", "action"] }
}})}

fn schema_get_unprocessed_conversations() -> Value { json!({ "type": "function", "function": {
    "name": "get_unprocessed_conversations",
    "description": "取得尚未處理的對話列表，用於記憶提煉",
    "parameters": { "type": "object", "properties": {
        "limit": { "type": "number", "description": "最多幾條，預設 20" }
    }, "required": [] }
}})}

fn schema_get_conversation_content() -> Value { json!({ "type": "function", "function": {
    "name": "get_conversation_content",
    "description": "取得指定對話的訊息內容",
    "parameters": { "type": "object", "properties": {
        "conversation_id": { "type": "string" },
        "skip_count":      { "type": "number", "description": "跳過前幾則" },
        "char_limit":      { "type": "number", "description": "字元限制，預設 500" }
    }, "required": ["conversation_id"] }
}})}

fn schema_save_memory_facts() -> Value { json!({ "type": "function", "function": {
    "name": "save_memory_facts",
    "description": "儲存從對話中提煉的記憶事實",
    "parameters": { "type": "object", "properties": {
        "conversation_id": { "type": "string" },
        "facts": { "type": "array", "items": { "type": "object" } }
    }, "required": ["conversation_id", "facts"] }
}})}

fn schema_mark_conversation_processed() -> Value { json!({ "type": "function", "function": {
    "name": "mark_conversation_processed",
    "description": "標記對話已完成記憶提煉",
    "parameters": { "type": "object", "properties": {
        "conversation_id": { "type": "string" }
    }, "required": ["conversation_id"] }
}})}

fn schema_condense_memory_facts() -> Value { json!({ "type": "function", "function": {
    "name": "condense_memory_facts",
    "description": "壓縮並合併同類記憶事實",
    "parameters": { "type": "object", "properties": {
        "category": { "type": "string", "description": "只壓縮指定類別，省略則全部" }
    }, "required": [] }
}})}

// ── Handler functions (one per tool) ─────────────────────────────────────────
//
// Simple tools delegate to dispatch_interactive_tool via the name macro.
// Special tools (plan_announce, open_note, call_agent) handle SSE / recursion directly.

/// Macro: generate a handler fn that calls dispatch_interactive_tool with a literal name.
macro_rules! make_handler {
    ($fn_name:ident, $tool_name:literal) => {
        fn $fn_name(env: Arc<VaultEnv>, args: Value) -> ToolFuture {
            Box::pin(async move {
                crate::service_agent::tools::vault_tools::dispatch_interactive_tool(
                    &env, $tool_name, &args,
                ).await.map(|(v, _)| v)
            })
        }
    };
}

make_handler!(handle_think,                        "think");
make_handler!(handle_list_structure,               "list_structure");
make_handler!(handle_read_note,                    "read_note");
make_handler!(handle_search_vault,                 "search_vault");
make_handler!(handle_query_memory,                 "query_memory");
make_handler!(handle_create_note,                  "create_note");
make_handler!(handle_create_folder,                "create_folder");
make_handler!(handle_update_note,                  "update_note");
make_handler!(handle_append_to_note,               "append_to_note");
make_handler!(handle_delete_note,                  "delete_note");
make_handler!(handle_delete_folder,                "delete_folder");
make_handler!(handle_move_note,                    "move_note");
make_handler!(handle_create_agent_skill,           "create_agent_skill");
make_handler!(handle_live_respond,                 "live_respond");
make_handler!(handle_get_unprocessed_conversations,"get_unprocessed_conversations");
make_handler!(handle_get_conversation_content,     "get_conversation_content");
make_handler!(handle_save_memory_facts,            "save_memory_facts");
make_handler!(handle_mark_conversation_processed,  "mark_conversation_processed");
make_handler!(handle_condense_memory_facts,        "condense_memory_facts");

// ── Special handlers ─────────────────────────────────────────────────────────

/// plan_announce: emit SSE only — no filesystem/DB side effects.
fn handle_plan_announce(env: Arc<VaultEnv>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let plan = args["plan"].as_str().unwrap_or("").to_string();
        env.state.daemon.emit("agent:plan_announce", json!({
            "session_id": env.session_id,
            "plan": plan,
        }));
        Ok(json!("✅ 已記錄計畫，請立即執行"))
    })
}

/// open_note: emit agent:open_note SSE and return opened paths.
fn handle_open_note(env: Arc<VaultEnv>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let paths: Vec<Value> = args["paths"].as_array()
            .cloned()
            .unwrap_or_else(|| {
                args["path"].as_str()
                    .map(|p| vec![json!(p)])
                    .unwrap_or_default()
            });
        env.state.daemon.emit("agent:open_note", json!(paths));
        Ok(json!({ "opened": paths }))
    })
}

/// call_agent: load agent def from DB, spawn a sub-agent, return its response.
fn handle_call_agent(env: Arc<VaultEnv>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let agent_name = args["name"].as_str().unwrap_or("").to_string();
        let input      = args["input"].as_str().unwrap_or("").to_string();
        if agent_name.is_empty() {
            return Err("call_agent: missing agent name".into());
        }
        let def = match crate::service_agent::helpers::load_agent_def(
            &env.db, &agent_name, &env.account_id,
        ).await {
            Some(d) => d,
            None => return Err(format!("call_agent: agent '{}' not found", agent_name)),
        };
        let result = crate::service_agent::agents::sub_agent::run_sub_agent(
            &env.state,
            &env.vault_id, &env.account_id,
            &env.session_id, &agent_name,
            def, &input,
            Arc::clone(&env.cancel),
        ).await;
        Ok(json!(result))
    })
}
