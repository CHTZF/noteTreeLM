use std::sync::Arc;

use serde_json::{json, Value};

use super::runtime::HarnessRequestRuntime;
use super::governance::guard::{GuardLevel, ToolGuardSpec, norm_path};
use crate::service::types::ToolFuture;
use super::tools::{memory_tools, vault_tools, trace_tools};

// ── ToolDef ───────────────────────────────────────────────────────────────────

/// Synchronous handler function pointer: takes the shared env + args, returns a boxed future.
/// Using fn pointer (not Box<dyn Fn>) keeps ToolDef Copy and zero-allocation.
pub(crate) type HandlerFn = fn(Arc<HarnessRequestRuntime>, Value) -> ToolFuture;

/// A single tool's complete definition: schema + guard + write-flag + handler + rollback,
/// all co-located. Adding a new tool means adding ONE entry here — no other files need touching.
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
    /// If Some, called (in reverse order) when a Transaction is cancelled after this tool ran.
    /// Receives the *same* args that were passed to `handler`.
    pub rollback:  Option<HandlerFn>,
}

// ── Static tool registry ──────────────────────────────────────────────────────

/// The canonical list of all interactive tools.
/// Schema, guard spec, write flag, and handler are defined together for each tool.
pub(crate) static ALL_TOOL_DEFS: &[ToolDef] = &[
    // ── Think (forced Round-0 reasoning; never in tool_names) ────────────────
    ToolDef { name: "think",          schema_fn: schema_think,          is_write: false, guard: None, handler: handle_think,          rollback: None },

    // ── Vault read tools ─────────────────────────────────────────────────────
    ToolDef { name: "list_structure", schema_fn: schema_list_structure, is_write: false, guard: None, handler: handle_list_structure, rollback: None },
    ToolDef { name: "read_note",        schema_fn: schema_read_note,        is_write: false, guard: None, handler: handle_read_note,        rollback: None },
    ToolDef { name: "search_in_note",  schema_fn: schema_search_in_note,  is_write: false, guard: None, handler: handle_search_in_note,  rollback: None },
    ToolDef { name: "search_vault",    schema_fn: schema_search_vault,    is_write: false, guard: None, handler: handle_search_vault,    rollback: None },
    ToolDef { name: "query_memory",          schema_fn: schema_query_memory,          is_write: false, guard: None, handler: handle_query_memory,          rollback: None },
    ToolDef { name: "get_current_datetime",  schema_fn: schema_get_current_datetime,  is_write: false, guard: None, handler: handle_get_current_datetime,  rollback: None },
    ToolDef { name: "list_recent_notes",     schema_fn: schema_list_recent_notes,     is_write: false, guard: None, handler: handle_list_recent_notes,     rollback: None },
    ToolDef { name: "search_by_tag",         schema_fn: schema_search_by_tag,         is_write: false, guard: None, handler: handle_search_by_tag,         rollback: None },
    ToolDef { name: "get_vault_stats",       schema_fn: schema_get_vault_stats,       is_write: false, guard: None, handler: handle_get_vault_stats,       rollback: None },
    ToolDef { name: "get_note_backlinks",    schema_fn: schema_get_note_backlinks,    is_write: false, guard: None, handler: handle_get_note_backlinks,    rollback: None },
    ToolDef { name: "find_orphan_notes",     schema_fn: schema_find_orphan_notes,     is_write: false, guard: None, handler: handle_find_orphan_notes,     rollback: None },

    // ── Composite read-write tools ───────────────────────────────────────────
    ToolDef { name: "read_then_write",  schema_fn: schema_read_then_write,  is_write: true,  guard: None, handler: handle_read_then_write, rollback: Some(rollback_overwrite_note) },

    // ── Vault write tools ────────────────────────────────────────────────────
    // create_note: rollback = delete the file (if it didn't exist before).
    ToolDef { name: "create_note",    schema_fn: schema_create_note,    is_write: true,  guard: None,                       handler: handle_create_note,    rollback: Some(rollback_create_note) },
    // create_folder: rollback = remove the directory.
    ToolDef { name: "create_folder",  schema_fn: schema_create_folder,  is_write: true,  guard: None,                       handler: handle_create_folder,  rollback: Some(rollback_create_folder) },
    // update_note: rollback = restore previous content (read before write in handler).
    ToolDef { name: "update_note",    schema_fn: schema_update_note,    is_write: true,  guard: Some(GUARD_UPDATE_NOTE),    handler: handle_update_note,    rollback: Some(rollback_overwrite_note) },
    // append_to_note: rollback = restore previous content (read before append in handler).
    ToolDef { name: "append_to_note", schema_fn: schema_append_to_note, is_write: true,  guard: Some(GUARD_APPEND_TO_NOTE), handler: handle_append_to_note, rollback: Some(rollback_overwrite_note) },
    // delete_note: rollback = restore deleted content (read before delete in handler).
    ToolDef { name: "delete_note",    schema_fn: schema_delete_note,    is_write: true,  guard: Some(GUARD_DELETE_NOTE),    handler: handle_delete_note,    rollback: Some(rollback_restore_note) },
    // delete_folder: not safely reversible (contents are gone); no rollback.
    ToolDef { name: "delete_folder",  schema_fn: schema_delete_folder,  is_write: true,  guard: Some(GUARD_DELETE_FOLDER),  handler: handle_delete_folder,  rollback: None },
    // move_note: rollback = move back.
    ToolDef { name: "move_note",               schema_fn: schema_move_note,               is_write: true,  guard: Some(GUARD_MOVE_NOTE),          handler: handle_move_note,               rollback: Some(rollback_move_note) },
    // update_note_frontmatter: rollback = restore previous content.
    ToolDef { name: "update_note_frontmatter", schema_fn: schema_update_note_frontmatter, is_write: true,  guard: Some(GUARD_UPDATE_FRONTMATTER), handler: handle_update_note_frontmatter, rollback: Some(rollback_overwrite_note) },
    // link_notes: rollback = restore source note content.
    ToolDef { name: "link_notes",            schema_fn: schema_link_notes,            is_write: true,  guard: Some(GUARD_LINK_NOTES),   handler: handle_link_notes,            rollback: Some(rollback_link_notes) },
    // compress_to_knowledge: rollback = delete the created knowledge note.
    ToolDef { name: "compress_to_knowledge", schema_fn: schema_compress_to_knowledge, is_write: true,  guard: None,                     handler: handle_compress_to_knowledge, rollback: Some(rollback_compress_to_knowledge) },
    // generate_moc: rollback = restore previous _moc.md or delete if newly created.
    ToolDef { name: "generate_moc",          schema_fn: schema_generate_moc,          is_write: true,  guard: Some(GUARD_GENERATE_MOC), handler: handle_generate_moc,          rollback: Some(rollback_generate_moc) },
    // schedule_task: rollback = delete the created task note.
    ToolDef { name: "schedule_task",         schema_fn: schema_schedule_task,         is_write: true,  guard: None,                     handler: handle_schedule_task,         rollback: Some(rollback_schedule_task) },

    // ── KB search (knowledge base import pages) ──────────────────────────────
    ToolDef { name: "search_kb_pages",     schema_fn: schema_search_kb_pages,     is_write: false, guard: None, handler: handle_search_kb_pages,     rollback: None },

    // ── Web search (Brave Search API) ────────────────────────────────────────
    ToolDef { name: "web_search",          schema_fn: schema_web_search,          is_write: false, guard: None, handler: handle_web_search,          rollback: None },

    // ── Skill search (live_chat agent) ───────────────────────────────────────
    ToolDef { name: "search_skills",       schema_fn: schema_search_skills,       is_write: false, guard: None, handler: handle_search_skills,       rollback: None },

    // ── Agent / UI tools ─────────────────────────────────────────────────────
    ToolDef { name: "get_session_state",   schema_fn: schema_get_session_state,   is_write: false, guard: None, handler: handle_get_session_state,   rollback: None },
    ToolDef { name: "compress_context",    schema_fn: schema_compress_context,    is_write: false, guard: None, handler: handle_compress_context,    rollback: None },
    ToolDef { name: "finish",              schema_fn: schema_finish,              is_write: false, guard: None, handler: handle_finish,              rollback: None },
    ToolDef { name: "ask_user",            schema_fn: schema_ask_user,            is_write: false, guard: None, handler: handle_ask_user,            rollback: None },
    ToolDef { name: "checkpoint",          schema_fn: schema_checkpoint,          is_write: false, guard: None, handler: handle_checkpoint,          rollback: None },
    ToolDef { name: "clear_checkpoint",    schema_fn: schema_clear_checkpoint,    is_write: false, guard: None, handler: handle_clear_checkpoint,    rollback: None },
    ToolDef { name: "progress",            schema_fn: schema_progress,            is_write: false, guard: None, handler: handle_progress,            rollback: None },
    ToolDef { name: "batch_apply",         schema_fn: schema_batch_apply,         is_write: true,  guard: None, handler: handle_batch_apply,         rollback: None },
    ToolDef { name: "save_agent_knowledge",schema_fn: schema_save_agent_knowledge,is_write: false, guard: None, handler: handle_save_agent_knowledge,rollback: None },
    ToolDef { name: "get_agent_knowledge", schema_fn: schema_get_agent_knowledge, is_write: false, guard: None, handler: handle_get_agent_knowledge, rollback: None },
    ToolDef { name: "get_vault_changes",   schema_fn: schema_get_vault_changes,   is_write: false, guard: None, handler: handle_get_vault_changes,   rollback: None },
    ToolDef { name: "plan_announce",       schema_fn: schema_plan_announce,       is_write: false, guard: None, handler: handle_plan_announce,       rollback: None },
    ToolDef { name: "open_note",           schema_fn: schema_open_note,           is_write: false, guard: None, handler: handle_open_note,           rollback: None },
    ToolDef { name: "create_agent_skill",  schema_fn: schema_create_agent_skill,  is_write: true,  guard: None, handler: handle_create_agent_skill,  rollback: None },
    ToolDef { name: "call_agent",          schema_fn: schema_call_agent,          is_write: false, guard: None, handler: handle_call_agent,          rollback: None },
    ToolDef { name: "live_respond",        schema_fn: schema_live_respond,        is_write: false, guard: None, handler: handle_live_respond,        rollback: None },

    // ── Memory agent tools ───────────────────────────────────────────────────
    ToolDef { name: "get_unprocessed_conversations", schema_fn: schema_get_unprocessed_conversations, is_write: false, guard: None, handler: handle_get_unprocessed_conversations, rollback: None },
    ToolDef { name: "get_conversation_content",      schema_fn: schema_get_conversation_content,      is_write: false, guard: None, handler: handle_get_conversation_content,      rollback: None },
    ToolDef { name: "save_memory_facts",             schema_fn: schema_save_memory_facts,             is_write: true,  guard: None, handler: handle_save_memory_facts,             rollback: None },
    ToolDef { name: "mark_conversation_processed",   schema_fn: schema_mark_conversation_processed,   is_write: true,  guard: None, handler: handle_mark_conversation_processed,   rollback: None },
    ToolDef { name: "condense_memory_facts",         schema_fn: schema_condense_memory_facts,         is_write: true,  guard: None, handler: handle_condense_memory_facts,         rollback: None },

    // ── Trace analyst tools (trace_analyst agent only) ───────────────────────
    ToolDef { name: "list_session_traces",            schema_fn: trace_tools::schema_list_session_traces,            is_write: false, guard: None, handler: trace_tools::handle_list_session_traces,            rollback: None },
    ToolDef { name: "read_session_with_conversation", schema_fn: trace_tools::schema_read_session_with_conversation, is_write: false, guard: None, handler: trace_tools::handle_read_session_with_conversation, rollback: None },
    ToolDef { name: "propose_eval_case",              schema_fn: trace_tools::schema_propose_eval_case,              is_write: true,  guard: None, handler: trace_tools::handle_propose_eval_case,              rollback: None },
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
const GUARD_UPDATE_FRONTMATTER: ToolGuardSpec = ToolGuardSpec {
    path_extractor: |args| args["path"].as_str().unwrap_or("").to_string(),
    require:        GuardLevel::ContentRead,
    is_folder:      false,
};
const GUARD_LINK_NOTES: ToolGuardSpec = ToolGuardSpec {
    path_extractor: |args| {
        let p = args["source"].as_str().unwrap_or("");
        let lower = p.to_lowercase();
        if lower.ends_with(".md") { lower } else { format!("{}.md", lower) }
    },
    require:    GuardLevel::ContentRead,
    is_folder:  false,
};
const GUARD_GENERATE_MOC: ToolGuardSpec = ToolGuardSpec {
    path_extractor: |args| args["path"].as_str().unwrap_or("").to_string(),
    require:    GuardLevel::PathSeen,
    is_folder:  true,
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
    "description": "列出 vault 的資料夾和筆記結構，每個 .md 檔案附帶檔案大小。大於 5 KB 的筆記建議用 read_note 的 offset/limit 分頁讀取，而非一次全讀。",
    "parameters": { "type": "object", "properties": {
        "path": { "type": "string", "description": "子路徑，省略則顯示根目錄" }
    }, "required": [] }
}})}

fn schema_read_note() -> Value { json!({ "type": "function", "function": {
    "name": "read_note",
    "description": "讀取指定路徑的筆記內容。回傳 {error_code, content, path, total_lines, has_more}：成功時 error_code 為 null；失敗時 error_code 為 NOT_FOUND / READ_FAILED。使用 offset + limit 分頁讀取長筆記，避免 context 暴增。",
    "parameters": { "type": "object", "properties": {
        "path":   { "type": "string", "description": "筆記的相對路徑（可省略 .md）" },
        "offset": { "type": "number", "description": "從第幾行開始讀取（0-indexed，省略則從頭讀）" },
        "limit":  { "type": "number", "description": "最多讀取幾行（省略則讀全文）" }
    }, "required": ["path"] }
}})}

fn schema_search_in_note() -> Value { json!({ "type": "function", "function": {
    "name": "search_in_note",
    "description": "在單一筆記中搜尋特定段落，返回命中段落和行號。當 read_note 回傳的內容過長時，用此工具精準定位目標段落，節省 context 用量。",
    "parameters": { "type": "object", "properties": {
        "path":  { "type": "string", "description": "筆記路徑（可省略 .md）" },
        "query": { "type": "string", "description": "要搜尋的關鍵字或片語" }
    }, "required": ["path", "query"] }
}})}

fn schema_search_vault() -> Value { json!({ "type": "function", "function": {
    "name": "search_vault",
    "description": "在 vault 中搜尋相關筆記",
    "parameters": { "type": "object", "properties": {
        "query": { "type": "string", "description": "搜尋關鍵字" }
    }, "required": ["query"] }
}})}

fn schema_web_search() -> Value { json!({ "type": "function", "function": {
    "name": "web_search",
    "description": "在網路上搜尋最新資訊（使用 Brave Search API）。當本地 vault 缺乏相關內容、或需要最新資訊時使用。不要用來查詢 Vault 筆記（請用 search_vault）。",
    "parameters": { "type": "object", "properties": {
        "query": { "type": "string", "description": "搜尋關鍵字或問題（建議使用具體關鍵字）" }
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

fn schema_update_note_frontmatter() -> Value { json!({ "type": "function", "function": {
    "name": "update_note_frontmatter",
    "description": "局部更新筆記的 YAML frontmatter 欄位，不覆蓋正文。若筆記無 frontmatter 則自動加上。",
    "parameters": { "type": "object", "properties": {
        "path":   { "type": "string", "description": "筆記路徑（可省略 .md）" },
        "fields": { "type": "object", "description": "要更新的鍵值對，例如 {\"tags\": [\"a\",\"b\"], \"status\": \"done\"}" }
    }, "required": ["path", "fields"] }
}})}

fn schema_get_current_datetime() -> Value { json!({ "type": "function", "function": {
    "name": "get_current_datetime",
    "description": "回傳目前的本地日期和時間字串",
    "parameters": { "type": "object", "properties": {}, "required": [] }
}})}

fn schema_list_recent_notes() -> Value { json!({ "type": "function", "function": {
    "name": "list_recent_notes",
    "description": "列出最近修改的筆記，依更新時間降序排列",
    "parameters": { "type": "object", "properties": {
        "limit": { "type": "number", "description": "最多幾條，預設 10，最多 50" }
    }, "required": [] }
}})}

fn schema_search_by_tag() -> Value { json!({ "type": "function", "function": {
    "name": "search_by_tag",
    "description": "搜尋含有指定 tag 的筆記（支援 #tag 行內標籤與 frontmatter tags 欄位）",
    "parameters": { "type": "object", "properties": {
        "tag": { "type": "string", "description": "要搜尋的標籤名稱（不含 #）" }
    }, "required": ["tag"] }
}})}

fn schema_get_vault_stats() -> Value { json!({ "type": "function", "function": {
    "name": "get_vault_stats",
    "description": "取得 vault 的統計資訊：筆記數量、資料夾數量",
    "parameters": { "type": "object", "properties": {}, "required": [] }
}})}

fn schema_get_note_backlinks() -> Value { json!({ "type": "function", "function": {
    "name": "get_note_backlinks",
    "description": "找出所有包含 [[notename]] wikilink 連結到指定筆記的其他筆記（反向連結）",
    "parameters": { "type": "object", "properties": {
        "path": { "type": "string", "description": "目標筆記路徑（可省略 .md）" }
    }, "required": ["path"] }
}})}

fn schema_find_orphan_notes() -> Value { json!({ "type": "function", "function": {
    "name": "find_orphan_notes",
    "description": "找出沒有被任何其他筆記 wikilink 引用的孤立筆記",
    "parameters": { "type": "object", "properties": {}, "required": [] }
}})}

fn schema_link_notes() -> Value { json!({ "type": "function", "function": {
    "name": "link_notes",
    "description": "在筆記 A 的「相關筆記」區塊中插入 [[筆記B]] wikilink，建立筆記間的雙向連結。",
    "parameters": { "type": "object", "properties": {
        "source": { "type": "string", "description": "要插入連結的筆記路徑（需已讀取）" },
        "target": { "type": "string", "description": "要被連結的目標筆記路徑" }
    }, "required": ["source", "target"] }
}})}

fn schema_compress_to_knowledge() -> Value { json!({ "type": "function", "function": {
    "name": "compress_to_knowledge",
    "description": "將重要的洞見或知識摘要儲存到 knowledge/ 資料夾，作為可查詢的長期知識庫。",
    "parameters": { "type": "object", "properties": {
        "title":   { "type": "string", "description": "知識筆記的標題（作為檔名）" },
        "content": { "type": "string", "description": "知識摘要內容（Markdown）" },
        "tags":    { "type": "array", "items": { "type": "string" }, "description": "標籤列表（選填）" }
    }, "required": ["title", "content"] }
}})}

fn schema_generate_moc() -> Value { json!({ "type": "function", "function": {
    "name": "generate_moc",
    "description": "為指定資料夾生成 Map of Contents（目錄索引），列出所有筆記的 wikilink，輸出至 _moc.md。",
    "parameters": { "type": "object", "properties": {
        "path":  { "type": "string", "description": "要生成 MOC 的資料夾路徑" },
        "title": { "type": "string", "description": "MOC 標題（省略則使用資料夾名稱）" }
    }, "required": ["path"] }
}})}

fn schema_schedule_task() -> Value { json!({ "type": "function", "function": {
    "name": "schedule_task",
    "description": "建立一個待辦任務，儲存到 tasks/ 資料夾，包含截止日期和狀態追蹤。",
    "parameters": { "type": "object", "properties": {
        "title":       { "type": "string", "description": "任務標題" },
        "description": { "type": "string", "description": "任務描述或執行步驟" },
        "due_date":    { "type": "string", "description": "截止日期（YYYY-MM-DD 格式，選填）" }
    }, "required": ["title", "description"] }
}})}

fn schema_search_skills() -> Value { json!({ "type": "function", "function": {
    "name": "search_skills",
    "description": "搜尋已建立的 agent 技能，找出符合當前意圖的行為與工具鏈",
    "parameters": { "type": "object", "properties": {
        "query": { "type": "string", "description": "搜尋關鍵字或描述" }
    }, "required": ["query"] }
}})}

fn schema_get_session_state() -> Value { json!({ "type": "function", "function": {
    "name": "get_session_state",
    "description": "回傳本輪 session 已執行的工具清單（name、args 摘要、guard 結果、耗時）及重複呼叫警告。當你不確定自己是否已讀取某個路徑、或要避免重複操作時，先呼叫此工具。",
    "parameters": { "type": "object", "properties": {} }
}})}

fn schema_compress_context() -> Value { json!({ "type": "function", "function": {
    "name": "compress_context",
    "description": "壓縮目前 context 中過長的工具結果，釋放 context 空間。保留所有 system 訊息、keep_ids 指定的工具呼叫結果、以及最近 4 則訊息，其餘替換成 summary。在 context 使用量超過 70% 時呼叫。",
    "parameters": { "type": "object", "properties": {
        "summary":  { "type": "string", "description": "已收集到的關鍵資訊摘要（條列格式，保留後續任務需要的內容）" },
        "keep_ids": { "type": "array", "items": { "type": "string" }, "description": "需要保留的 tool_call id 清單（這些工具結果不會被刪除）" }
    }, "required": ["summary"] }
}})}

fn schema_finish() -> Value { json!({ "type": "function", "function": {
    "name": "finish",
    "description": "明確宣告任務完成並提交最終回覆。呼叫後立即結束工具循環。當你已收集到足夠資訊、可以給出完整回覆時使用，避免多餘的 round。",
    "parameters": { "type": "object", "properties": {
        "answer": { "type": "string", "description": "給使用者的最終回覆（完整的 Markdown 格式）" }
    }, "required": ["answer"] }
}})}

fn schema_progress() -> Value { json!({ "type": "function", "function": {
    "name": "progress",
    "description": "回報長任務的執行進度，讓使用者知道目前完成了幾步、還剩多少。對需要多個步驟的任務，每完成一個子任務就呼叫一次。",
    "parameters": { "type": "object", "properties": {
        "current": { "type": "number", "description": "目前已完成的步驟數（從 1 開始）" },
        "total":   { "type": "number", "description": "任務總步驟數" },
        "message": { "type": "string", "description": "本步驟的說明（例如「已更新 notes/x.md」）" }
    }, "required": ["current", "total", "message"] }
}})}

fn schema_batch_apply() -> Value { json!({ "type": "function", "function": {
    "name": "batch_apply",
    "description": "對多個目標套用同一個工具操作，在單一 round 完成批次任務。比逐個呼叫更高效。支援的工具：read_note, search_in_note, update_note, append_to_note, read_then_write, update_note_frontmatter, delete_note。",
    "parameters": { "type": "object", "properties": {
        "tool":  { "type": "string", "description": "要套用的工具名稱（限上方支援清單）" },
        "items": {
            "type": "array",
            "description": "每個元素是傳給該工具的 args 物件（與單獨呼叫相同格式）",
            "items": { "type": "object" }
        }
    }, "required": ["tool", "items"] }
}})}

fn schema_save_agent_knowledge() -> Value { json!({ "type": "function", "function": {
    "name": "save_agent_knowledge",
    "description": "儲存關於此 vault 的操作知識或規則，供未來 session 使用。用於記錄使用者偏好、vault 結構規則、或常用操作模式。每個 key 對應一條知識，重複儲存同一 key 會更新。",
    "parameters": { "type": "object", "properties": {
        "key":     { "type": "string", "description": "知識識別鍵（例如 'naming_convention', 'folder_structure', 'user_preference'）" },
        "content": { "type": "string", "description": "知識內容（具體的規則或模式描述）" }
    }, "required": ["key", "content"] }
}})}

fn schema_get_agent_knowledge() -> Value { json!({ "type": "function", "function": {
    "name": "get_agent_knowledge",
    "description": "查詢已儲存的 vault 操作知識。當 context 中沒有注入知識、或需要查詢特定 key 時使用。",
    "parameters": { "type": "object", "properties": {
        "key": { "type": "string", "description": "要查詢的知識鍵（省略則返回所有知識）" }
    }, "required": [] }
}})}

fn schema_get_vault_changes() -> Value { json!({ "type": "function", "function": {
    "name": "get_vault_changes",
    "description": "返回自指定時間點以來在 vault 中被修改過的筆記清單。用於在兩次 session 之間偵測使用者手動修改了哪些檔案，以便 invalidate 舊的知識或重新讀取。",
    "parameters": { "type": "object", "properties": {
        "since_ts": { "type": "number", "description": "Unix 時間戳（秒），只返回 updated_at > since_ts 的筆記。省略則返回最近 24 小時的修改。" },
        "limit":    { "type": "number", "description": "最多返回幾筆，預設 20，最多 50" }
    }, "required": [] }
}})}

fn schema_read_then_write() -> Value { json!({ "type": "function", "function": {
    "name": "read_then_write",
    "description": "在單一工具呼叫中讀取並覆寫筆記。跳過 read_note → update_note 兩步流程，直接完成讀後寫，節省一個 round。回傳 diff 統計讓你確認寫入結果。",
    "parameters": { "type": "object", "properties": {
        "path":    { "type": "string", "description": "筆記路徑（可省略 .md）" },
        "content": { "type": "string", "description": "完整的新筆記內容（覆蓋原有內容）" }
    }, "required": ["path", "content"] }
}})}

fn schema_checkpoint() -> Value { json!({ "type": "function", "function": {
    "name": "checkpoint",
    "description": "儲存當前任務進度，下次對話開始時自動注入 context。當你完成一部分長任務、但任務尚未全部完成時呼叫，讓下次 session 知道從哪裡繼續。",
    "parameters": { "type": "object", "properties": {
        "summary":   { "type": "string", "description": "已完成的工作摘要（簡短條列）" },
        "remaining": { "type": "string", "description": "尚未完成的工作清單（條列格式，讓下次 agent 能直接執行）" }
    }, "required": ["summary", "remaining"] }
}})}

fn schema_clear_checkpoint() -> Value { json!({ "type": "function", "function": {
    "name": "clear_checkpoint",
    "description": "清除本對話的任務 checkpoint。當長任務全部完成後呼叫，避免舊進度繼續出現在 context 中。",
    "parameters": { "type": "object", "properties": {} }
}})}

fn schema_ask_user() -> Value { json!({ "type": "function", "function": {
    "name": "ask_user",
    "description": "向使用者提問，暫停執行等待補充資訊，收到回覆後自動繼續。當你需要更多資訊才能安全或正確地繼續任務時使用（例如：不確定目標路徑、操作範圍有歧義）。",
    "parameters": { "type": "object", "properties": {
        "question": { "type": "string", "description": "要問使用者的問題，具體說明你需要什麼資訊以及為什麼" }
    }, "required": ["question"] }
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

// ── Handler functions (one per tool, self-contained) ─────────────────────────
//
// Each handler receives Arc<HarnessRequestRuntime> + args and calls the appropriate vault
// function directly. No intermediary dispatcher — adding a new tool means
// adding ONE ToolDef entry here and nothing else.

// ── No-op tools ──────────────────────────────────────────────────────────────

fn handle_think(_env: Arc<HarnessRequestRuntime>, _args: Value) -> ToolFuture {
    Box::pin(async { Ok(json!("✅")) })
}

fn handle_live_respond(_env: Arc<HarnessRequestRuntime>, _args: Value) -> ToolFuture {
    // Args (speech/action/content) are consumed by the SSE layer in runner.rs.
    Box::pin(async { Ok(json!("✅")) })
}

// ── Read tools ────────────────────────────────────────────────────────────────

fn handle_list_structure(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path = args["path"].as_str().unwrap_or("");
        Ok(Value::String(
            vault_tools::vault_list_structure(path, &env.vault_path)
        ))
    })
}

fn handle_read_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let raw    = args["path"].as_str().unwrap_or("");
        let path   = norm_path(raw);
        let offset = args["offset"].as_u64().map(|v| v as usize);
        let limit  = args["limit"].as_u64().map(|v| v as usize);
        Ok(vault_tools::vault_read_note(&path, &env.vault_path, offset, limit))
    })
}

fn handle_search_in_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let raw   = args["path"].as_str().unwrap_or("");
        let query = args["query"].as_str().unwrap_or("");
        let path  = norm_path(raw);
        Ok(vault_tools::vault_search_in_note(&path, query, &env.vault_path))
    })
}

fn handle_get_vault_changes(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let since_ts = args["since_ts"].as_i64()
            .unwrap_or_else(|| chrono::Utc::now().timestamp() - 86_400);
        let limit = args["limit"].as_u64().unwrap_or(20).min(50);

        #[derive(serde::Deserialize)]
        struct Row { path: String, updated_at: Option<i64> }

        let mut resp = env.db
            .query("SELECT path, updated_at FROM notes \
                    WHERE vault_id = $vid AND updated_at > $since \
                    ORDER BY updated_at DESC LIMIT $lim")
            .bind(("vid",   env.vault_id.clone()))
            .bind(("since", since_ts))
            .bind(("lim",   limit))
            .await
            .map_err(|e| e.to_string())?;

        let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;
        if rows.is_empty() {
            return Ok(json!({ "changes": [], "since_ts": since_ts, "message": "此時間段內無修改" }));
        }
        let changes: Vec<Value> = rows.iter().map(|r| json!({
            "path":       r.path,
            "updated_at": r.updated_at,
        })).collect();
        Ok(json!({ "changes": changes, "count": changes.len(), "since_ts": since_ts }))
    })
}

fn handle_search_vault(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let query = args["query"].as_str().unwrap_or("");
        vault_tools::vault_search(
            &env.client, &env.embedding_url, &env.db, &env.vault_id, query,
        ).await
    })
}

fn schema_search_kb_pages() -> Value { json!({ "type": "function", "function": {
    "name": "search_kb_pages",
    "description": "搜尋知識庫已匯入頁面。若尚未匯入的相關頁面，會自動擷取後回傳。每個結果帶有 __cite_id__ 欄位，請在回答中用 [cite:id] 格式引用。",
    "parameters": { "type": "object", "properties": {
        "query": { "type": "string", "description": "搜尋關鍵字或問題" },
    }, "required": ["query"] }
}})}

fn handle_search_kb_pages(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let query = args["query"].as_str().unwrap_or("").to_string();
        vault_tools::vault_search_kb_pages(
            &env.client, &env.embedding_url, &env.db, &env.vault_id,
            env.source_type.as_deref(), env.source_id.as_deref(),
            &query,
        ).await
    })
}

fn handle_web_search(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let query = args["query"].as_str().unwrap_or("").to_string();
        if query.is_empty() {
            return Ok(json!("查詢不能為空"));
        }
        let session_id = env.session_id.clone();
        let emit_fn = env.emitter.as_emit_fn();
        vault_tools::vault_web_search(
            &env.client, &env.db,
            move |event: &str, payload| (emit_fn)(event.to_string(), payload),
            &session_id,
            &query,
        ).await
    })
}

fn handle_query_memory(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let keywords: Vec<String> = args["keywords"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
            .unwrap_or_default();
        let limit = args["limit"].as_u64().unwrap_or(5).min(20);
        vault_tools::vault_query_memory_with_ids(
            &env.client, &env.embedding_url, &env.db, &env.vault_id, &env.account_id,
            &keywords, limit,
        ).await.map(|(v, _)| v)
    })
}

// ── Write tools ───────────────────────────────────────────────────────────────

fn handle_read_then_write(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path    = norm_path(args["path"].as_str().unwrap_or(""));
        let content = args["content"].as_str().unwrap_or("").to_string();
        let full = std::path::Path::new(&env.vault_path).join(&path);
        // read_then_write reads the file itself; just pre-record mtime for snapshot key.
        let mtime_at_read = tokio::fs::metadata(&full).await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        env.record_mtime_if_absent(path.clone(), mtime_at_read).await;
        vault_tools::vault_read_then_write(
            &path, &content, mtime_at_read,
            &env.vault_path, &env.client, &env.db, &env.vault_id,
            &env.working_memory,
        ).await
    })
}

fn handle_create_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path    = norm_path(args["path"].as_str().unwrap_or(""));
        let content = args["content"].as_str().unwrap_or("");
        vault_tools::vault_create_note(
            &path, content, &env.vault_path, &env.client, &env.db, &env.vault_id,
        ).await
    })
}

fn handle_update_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path    = norm_path(args["path"].as_str().unwrap_or(""));
        let content = args["content"].as_str().unwrap_or("").to_string();
        // Read original once — used for rollback snapshot, diff, and conflict detection.
        let full = std::path::Path::new(&env.vault_path).join(&path);
        let original = tokio::fs::read_to_string(&full).await.unwrap_or_default();
        // Record mtime at read time for conflict detection.
        let mtime_at_read = tokio::fs::metadata(&full).await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        env.snapshot_and_mtime_if_absent(path.clone(), original.clone(), mtime_at_read).await;
        vault_tools::vault_update_note_with_conflict_check(
            &path, &content, &original, mtime_at_read,
            &env.vault_path, &env.client, &env.db, &env.vault_id,
        ).await
    })
}

fn handle_append_to_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path    = norm_path(args["path"].as_str().unwrap_or(""));
        let content = args["content"].as_str().unwrap_or("").to_string();
        // Snapshot original content before appending so rollback can restore it.
        let full = std::path::Path::new(&env.vault_path).join(&path);
        let original = tokio::fs::read_to_string(&full).await.unwrap_or_default();
        env.snapshot_if_absent(path.clone(), original).await;
        vault_tools::vault_append_to_note(
            &path, &content, &env.vault_path, &env.client, &env.db, &env.vault_id,
        ).await
    })
}

fn handle_create_folder(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path = args["path"].as_str().unwrap_or("");
        vault_tools::vault_create_folder(
            path, &env.vault_path, &env.db, &env.vault_id,
        ).await
    })
}

fn handle_delete_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path = norm_path(args["path"].as_str().unwrap_or(""));
        // Snapshot original content before deleting so rollback can restore it.
        let full = std::path::Path::new(&env.vault_path).join(&path);
        let original = tokio::fs::read_to_string(&full).await.unwrap_or_default();
        env.snapshot_if_absent(path.clone(), original).await;
        vault_tools::vault_delete_note(
            &path, &env.vault_path, &env.db, &env.vault_id,
        ).await
    })
}

fn handle_delete_folder(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path = args["path"].as_str().unwrap_or("");
        vault_tools::vault_delete_folder(
            path, &env.vault_path,
        ).await
    })
}

fn handle_move_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let from = norm_path(args["from"].as_str().unwrap_or(""));
        let to   = norm_path(args["to"].as_str().unwrap_or(""));
        vault_tools::vault_move_note(
            &from, &to, &env.vault_path, &env.client, &env.db, &env.vault_id,
        ).await
    })
}

fn handle_update_note_frontmatter(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path   = norm_path(args["path"].as_str().unwrap_or(""));
        let fields = args["fields"].clone();
        // Snapshot original content before modifying so rollback can restore it.
        let full = std::path::Path::new(&env.vault_path).join(&path);
        let original = tokio::fs::read_to_string(&full).await.unwrap_or_default();
        env.snapshot_if_absent(path.clone(), original).await;
        vault_tools::vault_update_note_frontmatter(
            &path, &fields, &env.vault_path, &env.client, &env.db, &env.vault_id,
        ).await
    })
}

fn handle_create_agent_skill(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let title          = args["title"].as_str().unwrap_or("").to_string();
        let trigger        = args["trigger"].as_str().unwrap_or("").to_string();
        let behavior       = args["behavior"].as_str().unwrap_or("").to_string();
        let injection_mode = args["injection_mode"].as_str().unwrap_or("passive").to_string();
        let skill_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();
        env.db
            .query("INSERT INTO agent_skills (skill_id, account_id, title, trigger, behavior, \
                    is_active, trigger_count, injection_mode, created_at) \
                    VALUES ($sid, $aid, $title, $trigger, $behavior, true, 0, $imode, $now)")
            .bind(("sid",   skill_id.clone()))
            .bind(("aid",   env.account_id.clone()))
            .bind(("title", title))
            .bind(("trigger", trigger))
            .bind(("behavior", behavior))
            .bind(("imode", injection_mode))
            .bind(("now",   now))
            .await
            .map_err(|e| e.to_string())?;
        // Trigger embedding in background so the new skill is searchable immediately.
        let db_c = env.db.clone();
        let aid  = env.account_id.clone();
        let eu   = env.embedding_url.clone();
        tokio::spawn(async move {
            crate::db::seeds::embed_skills_for_account(&db_c, &aid, &eu).await;
        });
        Ok(json!({ "ok": true, "skill_id": skill_id }))
    })
}

fn handle_get_current_datetime(_env: Arc<HarnessRequestRuntime>, _args: Value) -> ToolFuture {
    Box::pin(async {
        let now = chrono::Local::now();
        Ok(Value::String(now.format("%Y-%m-%d %H:%M:%S %z").to_string()))
    })
}

fn handle_list_recent_notes(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let limit = args["limit"].as_u64().unwrap_or(10);
        vault_tools::vault_list_recent_notes(&env.db, &env.vault_id, limit).await
    })
}

fn handle_search_by_tag(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let tag = args["tag"].as_str().unwrap_or("");
        vault_tools::vault_search_by_tag(&env.db, &env.vault_id, tag).await
    })
}

fn handle_get_vault_stats(env: Arc<HarnessRequestRuntime>, _args: Value) -> ToolFuture {
    Box::pin(async move {
        vault_tools::vault_get_stats(&env.db, &env.vault_id, &env.vault_path).await
    })
}

fn handle_get_note_backlinks(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path = norm_path(args["path"].as_str().unwrap_or(""));
        vault_tools::vault_get_note_backlinks(&env.db, &env.vault_id, &path).await
    })
}

fn handle_find_orphan_notes(env: Arc<HarnessRequestRuntime>, _args: Value) -> ToolFuture {
    Box::pin(async move {
        vault_tools::vault_find_orphan_notes(&env.db, &env.vault_id).await
    })
}

fn handle_link_notes(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let source = norm_path(args["source"].as_str().unwrap_or(""));
        let target = norm_path(args["target"].as_str().unwrap_or(""));
        // Snapshot source content before modification for rollback.
        let full = std::path::Path::new(&env.vault_path).join(&source);
        let original = tokio::fs::read_to_string(&full).await.unwrap_or_default();
        env.snapshot_if_absent(source.clone(), original).await;
        vault_tools::vault_link_notes(
            &source, &target, &env.vault_path, &env.client, &env.db, &env.vault_id,
        ).await
    })
}

fn handle_compress_to_knowledge(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let title   = args["title"].as_str().unwrap_or("").to_string();
        let content = args["content"].as_str().unwrap_or("").to_string();
        let tags: Vec<String> = args["tags"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        vault_tools::vault_compress_to_knowledge(
            &title, &content, &tags, &env.vault_path, &env.client, &env.db, &env.vault_id,
        ).await
    })
}

fn handle_generate_moc(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let folder = args["path"].as_str().unwrap_or("").to_string();
        let title  = args["title"].as_str().map(String::from);
        // Snapshot existing _moc.md (empty string if not present) so rollback can decide.
        let moc_rel = format!("{}/_moc.md", folder);
        let full = std::path::Path::new(&env.vault_path).join(&moc_rel);
        let original = tokio::fs::read_to_string(&full).await.unwrap_or_default();
        env.snapshot_if_absent(moc_rel, original).await;
        vault_tools::vault_generate_moc(
            &folder, title.as_deref(), &env.vault_path, &env.client, &env.db, &env.vault_id,
        ).await
    })
}

fn handle_schedule_task(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let title       = args["title"].as_str().unwrap_or("").to_string();
        let description = args["description"].as_str().unwrap_or("").to_string();
        let due_date    = args["due_date"].as_str().map(String::from);
        vault_tools::vault_schedule_task(
            &title, &description, due_date.as_deref(),
            &env.vault_path, &env.client, &env.db, &env.vault_id,
        ).await
    })
}

// ── Skill search ─────────────────────────────────────────────────────────────

fn handle_search_skills(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let query = args["query"].as_str().unwrap_or("").to_string();

        #[derive(serde::Deserialize)]
        struct Row {
            title:      String,
            behavior:   String,
            tool_calls: Option<Value>,
            embedding:  Option<Value>,
        }

        let mut resp = env.db
            .query("SELECT title, behavior, tool_calls, embedding \
                    FROM agent_skills WHERE account_id = $aid AND is_active = true LIMIT 30")
            .bind(("aid", env.account_id.clone()))
            .await
            .map_err(|e| e.to_string())?;
        let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;

        // Semantic search when we have a non-empty query and an embedding server.
        let q_vec = if !query.is_empty() {
            crate::embedding::embedder::embed_text(&env.client, &env.embedding_url, &query).await
        } else {
            None
        };

        let results: Vec<Value> = if let Some(ref qv) = q_vec {
            let mut scored: Vec<(f32, &Row)> = rows.iter().filter_map(|r| {
                let emb: Vec<f32> = r.embedding.as_ref()?.as_array()?
                    .iter().filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect();
                if emb.is_empty() { return None; }
                let score = crate::embedding::embedder::cosine_sim(qv, &emb);
                if score >= 0.60 { Some((score, r)) } else { None }
            }).collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            scored.into_iter().take(10).map(|(_, r)| json!({
                "title": r.title,
                "behavior": r.behavior,
                "required_tools": r.tool_calls,
            })).collect()
        } else {
            let q_lower = query.to_lowercase();
            rows.iter()
                .filter(|r| q_lower.is_empty()
                    || r.title.to_lowercase().contains(&q_lower)
                    || r.behavior.to_lowercase().contains(&q_lower))
                .take(10)
                .map(|r| json!({
                    "title": r.title,
                    "behavior": r.behavior,
                    "required_tools": r.tool_calls,
                }))
                .collect()
        };
        Ok(json!(results))
    })
}

// ── Agent / UI tools ──────────────────────────────────────────────────────────

/// get_session_state: snapshot WorkingMemory for agent self-inspection.
fn handle_get_session_state(env: Arc<HarnessRequestRuntime>, _args: Value) -> ToolFuture {
    Box::pin(async move {
        let calls = env.working_memory.snapshot_summary().await;
        let repeats = env.working_memory.repeated_calls().await;
        let repeat_warnings: Vec<Value> = repeats.iter().map(|(name, arg)| json!({
            "tool": name,
            "arg":  arg,
            "warning": format!("你已在本 session 多次呼叫 {}({})，請換策略或確認是否已得到所需資訊。", name, arg),
        })).collect();
        Ok(json!({
            "tool_calls": calls,
            "repeated_calls": repeat_warnings,
        }))
    })
}

fn handle_compress_context(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let summary = args["summary"].as_str().unwrap_or("").to_string();
        if summary.is_empty() {
            return Ok(json!("summary 不能為空"));
        }
        let keep_ids: Vec<String> = args["keep_ids"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let after = env.compress_msgs(&summary, 4, &keep_ids).await;
        Ok(json!(format!("✅ context 已壓縮，剩餘 {} 則訊息", after)))
    })
}

fn handle_finish(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let answer = args["answer"].as_str().unwrap_or("").to_string();
        if answer.is_empty() {
            return Ok(json!("answer 不能為空"));
        }
        env.set_finish_answer(answer).await;
        Ok(json!("✅"))
    })
}

fn handle_checkpoint(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let summary   = args["summary"].as_str().unwrap_or("").to_string();
        let remaining = args["remaining"].as_str().unwrap_or("").to_string();
        if summary.is_empty() && remaining.is_empty() {
            return Ok(json!("summary 和 remaining 不能都為空"));
        }
        let now = chrono::Utc::now().timestamp();
        // Delete any existing checkpoint for this conversation, then insert fresh.
        // Simpler than an upsert and avoids SurrealQL version compatibility issues.
        let _ = env.db
            .query("DELETE task_checkpoints WHERE conv_id = $cid")
            .bind(("cid", env.conv_id.clone()))
            .await;
        let _ = env.db
            .query("CREATE task_checkpoints CONTENT $data")
            .bind(("data", serde_json::json!({
                "conv_id":    env.conv_id,
                "account_id": env.account_id,
                "summary":    summary,
                "remaining":  remaining,
                "updated_at": now,
            })))
            .await;
        Ok(json!("✅ 進度已儲存，下次 session 開始時將自動載入"))
    })
}

fn handle_clear_checkpoint(env: Arc<HarnessRequestRuntime>, _args: Value) -> ToolFuture {
    Box::pin(async move {
        let _ = env.db
            .query("DELETE task_checkpoints WHERE conv_id = $cid")
            .bind(("cid", env.conv_id.clone()))
            .await;
        Ok(json!("✅ checkpoint 已清除"))
    })
}

fn handle_progress(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let current = args["current"].as_u64().unwrap_or(0);
        let total   = args["total"].as_u64().unwrap_or(0);
        let message = args["message"].as_str().unwrap_or("").to_string();
        env.emit("agent:progress", json!({
            "session_id": env.session_id,
            "current":    current,
            "total":      total,
            "message":    message,
        }));
        Ok(json!("✅"))
    })
}

fn handle_batch_apply(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        use super::governance::guard::evaluate_guard;

        let tool_name = args["tool"].as_str().unwrap_or("").to_string();
        let items     = args["items"].as_array().cloned().unwrap_or_default();

        // Whitelist: only tools that make semantic sense in batch.
        const ALLOWED: &[&str] = &[
            "read_note", "search_in_note", "update_note", "append_to_note",
            "read_then_write", "update_note_frontmatter", "delete_note",
        ];
        if !ALLOWED.contains(&tool_name.as_str()) {
            return Ok(json!(format!(
                "batch_apply 不支援 '{}'。支援的工具：{}",
                tool_name, ALLOWED.join(", ")
            )));
        }
        let def = match find_tool_def(&tool_name) {
            Some(d) => d,
            None    => return Ok(json!(format!("工具 '{}' 不存在", tool_name))),
        };

        let mut results: Vec<Value> = Vec::with_capacity(items.len());
        for (i, item_args) in items.iter().enumerate() {
            // Evaluate guard (same logic as build_interactive_registry).
            if let Some(ref spec) = def.guard {
                let hint = env.working_memory.with_records(|store| {
                    evaluate_guard(spec, item_args, store)
                }).await;
                if let Some(h) = hint {
                    results.push(json!({
                        "index":   i,
                        "blocked": true,
                        "reason":  h.message,
                        "required_tool": h.required_tool,
                        "required_path": h.required_path,
                    }));
                    continue;
                }
            }

            match (def.handler)(Arc::clone(&env), item_args.clone()).await {
                Ok(r)  => results.push(json!({ "index": i, "ok": true,  "result": r })),
                Err(e) => results.push(json!({ "index": i, "ok": false, "error":  e })),
            }
        }

        let ok_count      = results.iter().filter(|r| r["ok"] == json!(true)).count();
        let blocked_count = results.iter().filter(|r| r["blocked"] == json!(true)).count();
        let err_count     = results.iter().filter(|r| r["ok"] == json!(false)).count();

        Ok(json!({
            "results":       results,
            "total":         items.len(),
            "ok":            ok_count,
            "blocked":       blocked_count,
            "errors":        err_count,
        }))
    })
}

fn handle_save_agent_knowledge(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let key     = args["key"].as_str().unwrap_or("").to_string();
        let content = args["content"].as_str().unwrap_or("").to_string();
        if key.is_empty() {
            return Ok(json!("key 不能為空"));
        }
        let now = chrono::Utc::now().timestamp();
        // Delete existing entry for this vault+key, then insert fresh.
        let _ = env.db
            .query("DELETE agent_knowledge WHERE vault_id = $vid AND key = $key")
            .bind(("vid", env.vault_id.clone()))
            .bind(("key", key.clone()))
            .await;
        let _ = env.db
            .query("CREATE agent_knowledge CONTENT $data")
            .bind(("data", json!({
                "vault_id":   env.vault_id,
                "account_id": env.account_id,
                "key":        key.clone(),
                "content":    content,
                "updated_at": now,
            })))
            .await;
        Ok(json!(format!("✅ 已儲存知識：{}", key)))
    })
}

fn handle_get_agent_knowledge(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let key_filter = args["key"].as_str().map(String::from);

        #[derive(serde::Deserialize)]
        struct Row { key: String, content: String }

        let mut resp = if let Some(ref k) = key_filter {
            env.db
                .query("SELECT key, content FROM agent_knowledge \
                        WHERE vault_id = $vid AND key = $key LIMIT 1")
                .bind(("vid", env.vault_id.clone()))
                .bind(("key", k.clone()))
                .await
        } else {
            env.db
                .query("SELECT key, content FROM agent_knowledge \
                        WHERE vault_id = $vid ORDER BY updated_at DESC")
                .bind(("vid", env.vault_id.clone()))
                .await
        }.map_err(|e| e.to_string())?;

        let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;
        if rows.is_empty() {
            return Ok(json!("（尚無儲存的 agent 知識）"));
        }
        let entries: Vec<Value> = rows.iter()
            .map(|r| json!({ "key": r.key, "content": r.content }))
            .collect();
        Ok(json!(entries))
    })
}

/// ask_user: suspend the agent and wait for the user to reply via the chat box.
///
/// The question is emitted as `llm:done` so the frontend renders it as an assistant
/// message and exits loading state (allowing the user to type).  The tool then
/// blocks on a oneshot channel; `run_agent` detects `waiting_for_answer` on the
/// next user message and forwards it to this channel to resume execution.
fn handle_ask_user(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        use std::sync::atomic::Ordering;

        let question = args["question"].as_str().unwrap_or("").to_string();
        if question.is_empty() {
            return Ok(json!("問題不能為空"));
        }

        // Register via AnswerChannel — no &mut session needed.
        let rx = env.answer_channel.wait().await;

        // Emit the question as a regular assistant message so the frontend
        // exits loading state and lets the user type their reply.
        env.emit("llm:done", serde_json::json!(question));

        // Wait for the answer or cancellation.
        let cancel = Arc::clone(&env.cancel);
        tokio::select! {
            result = rx => {
                Ok(json!(result.unwrap_or_default()))
            }
            _ = async {
                loop {
                    if cancel.load(Ordering::Relaxed) { break; }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            } => {
                Ok(json!("（使用者取消）"))
            }
        }
    })
}

/// plan_announce: emit SSE only — no filesystem/DB side effects.
fn handle_plan_announce(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let plan = args["plan"].as_str().unwrap_or("").to_string();
        env.emit("agent:plan_announce", json!({ "plan": plan }));
        Ok(json!("✅ 已記錄計畫，請立即執行"))
    })
}

/// open_note: emit agent:open_note SSE and return opened paths.
fn handle_open_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let paths: Vec<Value> = args["paths"].as_array()
            .cloned()
            .unwrap_or_else(|| {
                args["path"].as_str()
                    .map(|p| vec![json!(p)])
                    .unwrap_or_default()
            });
        env.emit("agent:open_note", json!(paths));
        Ok(json!({ "opened": paths }))
    })
}

/// call_agent: load agent def from DB, spawn a sub-agent, return its response.
fn handle_call_agent(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let agent_name = args["name"].as_str().unwrap_or("").to_string();
        let input      = args["input"].as_str().unwrap_or("").to_string();
        if agent_name.is_empty() {
            return Err("call_agent: missing agent name".into());
        }
        let def = match crate::service::helpers::load_agent_def(
            &env.db, &agent_name, &env.account_id,
        ).await {
            Some(d) => d,
            None => return Err(format!("call_agent: agent '{}' not found", agent_name)),
        };
        let result = crate::service::agents::sub_agent::run_sub_agent(
            &env,
            &env.session_id, &agent_name,
            def, &input,
            Arc::clone(&env.cancel),
        ).await;
        Ok(json!(result))
    })
}

// ── Memory agent tools ────────────────────────────────────────────────────────

fn handle_get_unprocessed_conversations(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let limit = args["limit"].as_i64().unwrap_or(20);
        memory_tools::get_unprocessed_conversations(
            &env.db, &env.vault_id, &env.account_id, limit,
        ).await
    })
}

fn handle_get_conversation_content(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let conv_id    = args["conversation_id"].as_str().unwrap_or("").to_string();
        let skip       = args["skip_count"].as_i64().unwrap_or(0);
        let char_limit = args["char_limit"].as_i64().unwrap_or(500);
        memory_tools::get_conversation_content(
            &env.db, &conv_id, skip, char_limit,
        ).await
    })
}

fn handle_save_memory_facts(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
        let facts   = args["facts"].as_array().cloned().unwrap_or_default();
        memory_tools::save_memory_facts(
            &env.client, &env.db, &env.vault_id, &env.account_id, &conv_id, facts,
            &env.embedding_url,
        ).await
    })
}

fn handle_mark_conversation_processed(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
        memory_tools::mark_conversation_processed(
            &env.db, &conv_id,
        ).await
    })
}

fn handle_condense_memory_facts(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let category = args["category"].as_str().map(String::from);
        memory_tools::condense_memory_facts(
            &env.client, &env.llm_url, &env.db, &env.vault_id, &env.account_id,
            category, &env.embedding_url,
        ).await
    })
}

// ── Rollback handlers ─────────────────────────────────────────────────────────
//
// Each fn receives the *original tool args* (same Value passed to the forward handler).
// For update/append/delete: the forward handler snapshots pre-write content into
// env.write_snapshots (keyed by normalized path) before mutating the file;
// the rollback fn reads from the snapshot to restore.
// For create_note / create_folder: args alone are sufficient (just delete/rmdir).

/// Rollback create_note: delete the newly created file.
fn rollback_create_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path = norm_path(args["path"].as_str().unwrap_or(""));
        if path == ".md" { return Ok(json!(null)); }
        let full = std::path::Path::new(&env.vault_path).join(&path);
        let _ = tokio::fs::remove_file(&full).await;
        Ok(json!(null))
    })
}

/// Rollback create_folder: remove the directory.
fn rollback_create_folder(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path = args["path"].as_str().unwrap_or("");
        if path.is_empty() { return Ok(json!(null)); }
        let full = std::path::Path::new(&env.vault_path).join(path);
        let _ = tokio::fs::remove_dir_all(&full).await;
        Ok(json!(null))
    })
}

/// Rollback update_note / append_to_note: restore original content.
/// The forward handler snapshotted the pre-write content in env.write_snapshots.
/// If no snapshot exists (e.g. file was new) the rollback is a no-op.
fn rollback_overwrite_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path     = norm_path(args["path"].as_str().unwrap_or(""));
        let original = match env.get_snapshot(&path).await {
            Some(s) => s,
            None    => return Ok(json!(null)),
        };
        let full = std::path::Path::new(&env.vault_path).join(&path);
        let _ = tokio::fs::write(&full, &original).await;
        vault_tools::sync_note_to_db(&env.client, &env.db, &env.vault_id, &path, &original).await;
        Ok(json!(null))
    })
}

/// Rollback delete_note: restore the deleted file.
/// The forward handler snapshotted the pre-delete content in env.write_snapshots.
fn rollback_restore_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let path     = norm_path(args["path"].as_str().unwrap_or(""));
        let original = match env.get_snapshot(&path).await {
            Some(s) => s,
            None    => return Ok(json!(null)),
        };
        let full = std::path::Path::new(&env.vault_path).join(&path);
        if let Some(parent) = full.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::write(&full, &original).await;
        vault_tools::sync_note_to_db(&env.client, &env.db, &env.vault_id, &path, &original).await;
        Ok(json!(null))
    })
}

/// Rollback move_note: move back (to → from).
/// args contains "from" (original source) and "to" (original dest, now current location).
fn rollback_move_note(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let from = norm_path(args["from"].as_str().unwrap_or(""));
        let to   = norm_path(args["to"].as_str().unwrap_or(""));
        if from == ".md" || to == ".md" { return Ok(json!(null)); }
        // After the forward move: file is at `to`. Move it back to `from`.
        vault_tools::vault_move_note(
            &to, &from, &env.vault_path, &env.client, &env.db, &env.vault_id,
        ).await.ok();
        Ok(json!(null))
    })
}

/// Rollback link_notes: restore source note's original content from snapshot.
fn rollback_link_notes(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let source = norm_path(args["source"].as_str().unwrap_or(""));
        let original = match env.get_snapshot(&source).await {
            Some(s) => s,
            None    => return Ok(json!(null)),
        };
        let full = std::path::Path::new(&env.vault_path).join(&source);
        let _ = tokio::fs::write(&full, &original).await;
        vault_tools::sync_note_to_db(&env.client, &env.db, &env.vault_id, &source, &original).await;
        Ok(json!(null))
    })
}

/// Rollback compress_to_knowledge: delete the created knowledge note.
fn rollback_compress_to_knowledge(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let title = args["title"].as_str().unwrap_or("");
        if title.is_empty() { return Ok(json!(null)); }
        let safe = title.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_").to_lowercase();
        let full = std::path::Path::new(&env.vault_path).join(format!("knowledge/{}.md", safe));
        let _ = tokio::fs::remove_file(&full).await;
        Ok(json!(null))
    })
}

/// Rollback generate_moc: restore previous _moc.md if it existed, or delete if newly created.
fn rollback_generate_moc(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let folder = args["path"].as_str().unwrap_or("").to_string();
        if folder.is_empty() { return Ok(json!(null)); }
        let moc_rel = format!("{}/_moc.md", folder);
        let original = env.get_snapshot(&moc_rel).await;
        let full = std::path::Path::new(&env.vault_path).join(&moc_rel);
        match original {
            Some(s) if !s.is_empty() => {
                let _ = tokio::fs::write(&full, &s).await;
                vault_tools::sync_note_to_db(&env.client, &env.db, &env.vault_id, &moc_rel, &s).await;
            }
            _ => { let _ = tokio::fs::remove_file(&full).await; }
        }
        Ok(json!(null))
    })
}

/// Rollback schedule_task: delete the created task note.
fn rollback_schedule_task(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let title = args["title"].as_str().unwrap_or("");
        if title.is_empty() { return Ok(json!(null)); }
        let safe = title.replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_").to_lowercase();
        let full = std::path::Path::new(&env.vault_path).join(format!("tasks/{}.md", safe));
        let _ = tokio::fs::remove_file(&full).await;
        Ok(json!(null))
    })
}

