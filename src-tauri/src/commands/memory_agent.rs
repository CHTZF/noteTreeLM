/// commands/memory_agent.rs — 記憶提取 Sub-Agent
///
/// 架構：
///   - 5 個 ToolHandler 註冊進 ToolRegistry，供 LLM 呼叫
///   - run_memory_agent_loop(app) 建立 llm_fn + registry，呼叫 run_sub_agent
///   - start_memory_agent_scheduler(app) 每 8 小時觸發一次
///   - trigger_memory_agent Tauri command 供前端手動呼叫

use crate::{error::AppError, state::AppState};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tauri::State;
use tokio::io::AsyncWriteExt;

// ─── 資料結構 ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConversationPreview {
    pub id: String,
    pub title: String,
    pub message_count: i64,
    pub preview: String,
    pub updated_at: i64,
}

// ─── 內部工具函式（ToolHandler 閉包呼叫，也可被 Tauri command 呼叫） ──────────

pub(crate) async fn internal_get_unprocessed(
    db: &crate::db::surreal::SurrealDb,
    vault_id: &str,
    limit: i64,
) -> Vec<ConversationPreview> {
    #[derive(Deserialize)]
    struct Row {
        id: String,
        title: String,
        messages_json: String,
        updated_at: surrealdb::sql::Datetime,
    }

    let mut resp = match db.query(
        "SELECT record::id(id) AS id, title, messages_json, updated_at
         FROM conversations
         WHERE vault_id = $vid AND (memory_processed = false OR memory_processed IS NONE)
         ORDER BY updated_at DESC
         LIMIT $limit"
    )
    .bind(("vid", vault_id.to_owned()))
    .bind(("limit", limit))
    .await { Ok(r) => r, Err(_) => return vec![] };

    let rows: Vec<Row> = resp.take(0).unwrap_or_default();

    rows.into_iter().map(|row| {
        let msgs: serde_json::Value = serde_json::from_str(&row.messages_json)
            .unwrap_or(serde_json::json!([]));
        let arr = msgs.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
        let message_count = arr.iter()
            .filter(|m| matches!(m["role"].as_str(), Some("user") | Some("assistant")))
            .count() as i64;
        let preview: String = arr.iter()
            .filter(|m| m["role"].as_str() == Some("user"))
            .take(2)
            .filter_map(|m| m["content"].as_str())
            .map(|s| s.chars().take(100).collect::<String>())
            .collect::<Vec<_>>()
            .join(" / ");
        ConversationPreview {
            id: row.id,
            title: row.title,
            message_count,
            preview,
            updated_at: row.updated_at.timestamp(),
        }
    }).collect()
}

pub(crate) async fn internal_get_content(
    db: &crate::db::surreal::SurrealDb,
    conversation_id: &str,
) -> String {
    #[derive(Deserialize)]
    struct Row { messages_json: String }
    let mut resp = match db.query(
        "SELECT messages_json FROM type::thing('conversations', $id)"
    ).bind(("id", conversation_id.to_owned())).await {
        Ok(r) => r, Err(_) => return String::new(),
    };
    let row: Option<Row> = resp.take(0).unwrap_or_default();
    let row = match row { Some(r) => r, None => return String::new() };
    let msgs: serde_json::Value = serde_json::from_str(&row.messages_json)
        .unwrap_or(serde_json::json!([]));
    msgs.as_array().unwrap_or(&vec![]).iter()
        .filter_map(|m| {
            let role = m["role"].as_str()?;
            let content = m["content"].as_str()?;
            match role {
                "user"      => Some(format!("使用者：{}", content.chars().take(500).collect::<String>())),
                "assistant" => Some(format!("助理：{}", content.chars().take(800).collect::<String>())),
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) async fn internal_call_claude(vault_path: &str, prompt: &str) -> Result<String, String> {
    let which = tokio::process::Command::new("which")
        .arg("claude").output().await;
    let cli_path = match which {
        Ok(out) if out.status.success() =>
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => return Err("Claude CLI 未找到，請先安裝：npm install -g @anthropic-ai/claude-code".to_string()),
    };

    let mut cmd = tokio::process::Command::new(&cli_path);
    cmd.args(["-p", prompt, "--output-format", "text"]);
    if !vault_path.is_empty() {
        cmd.args(["--add-dir", vault_path]);
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        cmd.output(),
    ).await
    .map_err(|_| "Claude CLI 執行逾時（120s）".to_string())?
    .map_err(|e| format!("Claude CLI 啟動失敗：{}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!("Claude CLI 執行失敗：{}", String::from_utf8_lossy(&output.stderr).trim()))
    }
}

pub(crate) async fn internal_write_log(vault_path: &str, level: &str, message: &str) {
    if vault_path.is_empty() { return; }
    let log_dir = PathBuf::from(vault_path).join("log");
    let _ = tokio::fs::create_dir_all(&log_dir).await;
    let today = Local::now().format("%Y%m%d").to_string();
    let log_path = log_dir.join(format!("memory_agent_{}.log", today));
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let line = format!("[{}] [{}] {}\n", timestamp, level.to_uppercase(), message);
    if let Ok(mut file) = tokio::fs::OpenOptions::new()
        .create(true).append(true).open(&log_path).await
    {
        let _ = file.write_all(line.as_bytes()).await;
    }
}

pub(crate) async fn internal_mark_processed(
    db: &crate::db::surreal::SurrealDb,
    conversation_id: &str,
) {
    let _ = db.query(
        "UPDATE type::thing('conversations', $id) SET memory_processed = true, updated_at = time::now()"
    ).bind(("id", conversation_id.to_owned())).await;
}

// ─── Tauri Commands（前端手動呼叫） ──────────────────────────────────────────

#[tauri::command]
pub async fn get_unprocessed_conversations(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> Result<Vec<ConversationPreview>, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    Ok(internal_get_unprocessed(db, &vault_id, limit.unwrap_or(20) as i64).await)
}

#[tauri::command]
pub async fn get_conversation_content(
    state: State<'_, AppState>,
    conversation_id: String,
) -> Result<String, AppError> {
    Ok(internal_get_content(&state.db, &conversation_id).await)
}

#[tauri::command]
pub async fn call_claude_cli(
    state: State<'_, AppState>,
    prompt: String,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;
    internal_call_claude(&vault_path, &prompt).await
        .map_err(AppError::AI)
}

#[tauri::command]
pub async fn write_memory_log(
    state: State<'_, AppState>,
    level: String,
    message: String,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    internal_write_log(&vault_path, &level, &message).await;
    Ok(())
}

#[tauri::command]
pub async fn mark_conversation_processed(
    state: State<'_, AppState>,
    conversation_id: String,
) -> Result<(), AppError> {
    internal_mark_processed(&state.db, &conversation_id).await;
    Ok(())
}

/// 手動觸發記憶 sub-agent（前端設定頁按鈕）
#[tauri::command]
pub async fn trigger_memory_agent(app: tauri::AppHandle) -> Result<(), AppError> {
    tokio::spawn(async move {
        run_memory_agent_loop(&app).await;
    });
    Ok(())
}

// ─── Agent 工具定義（JSON schema，供 LLM 看到） ──────────────────────────────

pub fn memory_agent_tools() -> Vec<serde_json::Value> {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "get_unprocessed_conversations",
                "description": "取得尚未分析記憶的對話列表（含標題與內容預覽）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": { "type": "number", "description": "最多取幾筆，預設 20" }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_conversation_content",
                "description": "取得指定對話的完整訊息，用於判斷是否有記憶價值",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "conversation_id": { "type": "string" }
                    },
                    "required": ["conversation_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "call_claude_cli",
                "description": "呼叫 Claude Code CLI 提取高品質記憶事實。僅對有記憶價值的對話呼叫。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string", "description": "傳給 Claude 的提取指令" }
                    },
                    "required": ["prompt"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_memory_log",
                "description": "寫入記憶系統日誌（INFO/WARN/ERROR）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "level":   { "type": "string", "description": "INFO | WARN | ERROR" },
                        "message": { "type": "string" }
                    },
                    "required": ["level", "message"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "mark_conversation_processed",
                "description": "標記對話已完成記憶分析（無論有無記憶價值都必須標記）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "conversation_id": { "type": "string" }
                    },
                    "required": ["conversation_id"]
                }
            }
        }
    ]).as_array().unwrap().clone()
}

// ─── 建立 ToolRegistry（僅包含記憶提取工具） ─────────────────────────────────

fn build_memory_registry(
    db: crate::db::surreal::SurrealDb,
    vault_id: String,
    vault_path: String,
) -> Arc<crate::runtime::tool_registry::ToolRegistry> {
    use crate::runtime::tool_registry::ToolRegistry;
    use crate::runtime::types::Tool;

    let mut registry = ToolRegistry::new();

    // get_unprocessed_conversations
    {
        let db = db.clone(); let vid = vault_id.clone();
        registry.register("get_unprocessed_conversations".into(), Tool {
            execute: Arc::new(move |args| {
                let db = db.clone(); let vid = vid.clone();
                let limit = args["limit"].as_i64().unwrap_or(20);
                Box::pin(async move {
                    let previews = internal_get_unprocessed(&db, &vid, limit).await;
                    Ok(serde_json::to_value(previews).unwrap_or(serde_json::json!([])))
                })
            }),
            rollback: None,
        });
    }

    // get_conversation_content
    {
        let db = db.clone();
        registry.register("get_conversation_content".into(), Tool {
            execute: Arc::new(move |args| {
                let db = db.clone();
                let cid = args["conversation_id"].as_str().unwrap_or("").to_string();
                Box::pin(async move {
                    Ok(serde_json::Value::String(internal_get_content(&db, &cid).await))
                })
            }),
            rollback: None,
        });
    }

    // call_claude_cli
    {
        let vp = vault_path.clone();
        registry.register("call_claude_cli".into(), Tool {
            execute: Arc::new(move |args| {
                let vp = vp.clone();
                let prompt = args["prompt"].as_str().unwrap_or("").to_string();
                Box::pin(async move {
                    match internal_call_claude(&vp, &prompt).await {
                        Ok(out) => Ok(serde_json::Value::String(out)),
                        Err(e)  => Ok(serde_json::Value::String(format!("ERROR: {}", e))),
                    }
                })
            }),
            rollback: None,
        });
    }

    // write_memory_log
    {
        let vp = vault_path.clone();
        registry.register("write_memory_log".into(), Tool {
            execute: Arc::new(move |args| {
                let vp = vp.clone();
                let level = args["level"].as_str().unwrap_or("INFO").to_string();
                let msg   = args["message"].as_str().unwrap_or("").to_string();
                Box::pin(async move {
                    internal_write_log(&vp, &level, &msg).await;
                    Ok(serde_json::Value::String("ok".into()))
                })
            }),
            rollback: None,
        });
    }

    // mark_conversation_processed
    {
        let db = db.clone();
        registry.register("mark_conversation_processed".into(), Tool {
            execute: Arc::new(move |args| {
                let db = db.clone();
                let cid = args["conversation_id"].as_str().unwrap_or("").to_string();
                Box::pin(async move {
                    internal_mark_processed(&db, &cid).await;
                    Ok(serde_json::Value::String("ok".into()))
                })
            }),
            rollback: None,
        });
    }

    Arc::new(registry)
}

// ─── 排程 Agent 通用入口 ──────────────────────────────────────────────────────

/// 排程器觸發時呼叫，根據 agent_type 路由到對應的執行邏輯
pub async fn run_scheduled_agent(
    app: &tauri::AppHandle,
    agent_type: Option<String>,
    agent_prompt: Option<String>,
    description: String,
) {
    use tauri::Manager;
    match agent_type.as_deref() {
        Some("memory_agent") => {
            run_memory_agent_loop(app).await;
        }
        Some(unknown_type) => {
            // 未知 agent_type：用 vault registry 跑通用 sub-agent
            let state = app.state::<AppState>();
            let vault_path = state.get_vault_path().await;
            let vault_id = state.get_vault_id().await.unwrap_or_default();
            if vault_path.is_empty() { return; }

            let base_url = {
                let port = *state.llama_actual_port.lock().await;
                match port {
                    Some(p) => format!("http://127.0.0.1:{}", p),
                    None => return,
                }
            };

            let client = state.http_client.clone();
            let app_clone = app.clone();
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let cancel_clone = Arc::clone(&cancel);
            let base_url_for_registry = base_url.clone();

            let llm_fn: crate::runtime::types::LlmFn = {
                use crate::commands::ai::{send_streaming_request, detect_tool_calls};
                use crate::runtime::types::LlmRound;
                Arc::new(move |msgs, tools_opt, _cancel| {
                    let client = client.clone();
                    let base = base_url.clone();
                    let app = app_clone.clone();
                    let cancel2 = Arc::clone(&cancel_clone);
                    Box::pin(async move {
                        let body = if let Some(tools) = tools_opt {
                            serde_json::json!({
                                "messages": msgs, "tools": tools,
                                "tool_choice": "auto", "max_tokens": 2048,
                                "temperature": 0.3, "stream": true,
                            })
                        } else {
                            serde_json::json!({
                                "messages": msgs, "max_tokens": 2048,
                                "temperature": 0.3, "stream": true,
                            })
                        };
                        let result = send_streaming_request(&client, &base, body, &app, Some(cancel2))
                            .await.map_err(|e| e.to_string())?;
                        let tool_calls = detect_tool_calls(&result);
                        Ok(LlmRound { full_text: result.full_text, tool_calls })
                    })
                })
            };

            let registry = crate::tools::build_vault_registry(
                vault_path.clone(),
                state.db.clone(),
                vault_id,
                state.sqlite.clone(),
                app.clone(),
                Some(base_url_for_registry),
                Arc::clone(&state.search_method_tx),
                crate::tools::make_late_llm_fn(),
                Arc::new(tokio::sync::Mutex::new(None)),
                Arc::clone(&state.system_agent),
                Some(cancel),
                Arc::clone(&state.api_key_cache),
                Arc::clone(&state.settings_cache),
            );

            let emit: crate::runtime::types::EmitEventFn = Arc::new(|_, _| {});
            let tools_json = serde_json::Value::Array(
                crate::commands::ai::vault_tools().as_array().cloned().unwrap_or_default()
            );
            let task = agent_prompt.unwrap_or(description);

            let _ = crate::runtime::sub_agent::run_sub_agent(
                uuid::Uuid::new_v4().to_string(),
                "scheduler".to_string(),
                unknown_type,
                &task,
                "",
                tools_json,
                registry,
                llm_fn,
                emit,
            ).await;
        }
        None => {
            // 無 agent_type：不應發生（排程任務都應指定 agent_type）
            eprintln!("[scheduler] task has no agent_type, description={}", description);
        }
    }
}

// ─── 核心 Agent 執行迴圈 ─────────────────────────────────────────────────────

const SYSTEM_PROMPT: &str = "\
你是記憶管理助理。任務：
1. 呼叫 get_unprocessed_conversations 取得待分析對話列表
2. 對每個對話呼叫 get_conversation_content 取得內容
3. 判斷是否有長期記憶價值（使用者偏好/背景/規則/重要決策）
   - 一般查詢、搜尋、閒聊 → 無記憶價值
   - 使用者分享個人資訊、設定偏好、做重要決定 → 有記憶價值
4. 有價值 → 呼叫 call_claude_cli 提取事實
   失敗 → 呼叫 write_memory_log（level: ERROR）
5. 每個對話無論成功失敗 → 呼叫 mark_conversation_processed
6. 全部完成後 → 呼叫 write_memory_log 記錄摘要（level: INFO）";

pub async fn run_memory_agent_loop(app: &tauri::AppHandle) {
    use crate::commands::ai::{send_streaming_request, detect_tool_calls};
    use crate::runtime::sub_agent::run_sub_agent;
    use crate::runtime::types::LlmRound;
    use tauri::Manager;

    let state = app.state::<AppState>();

    let vault_path = state.get_vault_path().await;
    let vault_id = match state.get_vault_id().await {
        Ok(id) => id,
        Err(_) => return,
    };

    // vault 未設定時跳過，並記 log
    if vault_path.is_empty() {
        return;
    }

    // llama-server 必須在線
    let base_url = {
        let port = *state.llama_actual_port.lock().await;
        match port {
            Some(p) => format!("http://127.0.0.1:{}", p),
            None => {
                internal_write_log(&vault_path, "WARN",
                    "llama-server 未啟動，跳過本次記憶分析").await;
                return;
            }
        }
    };

    let client = state.http_client.clone();
    let app_clone = app.clone();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_clone = Arc::clone(&cancel);

    let llm_fn: crate::runtime::types::LlmFn = Arc::new(move |msgs, tools_opt, _cancel| {
        let client = client.clone();
        let base = base_url.clone();
        let app = app_clone.clone();
        let cancel2 = Arc::clone(&cancel_clone);
        Box::pin(async move {
            let body = if let Some(tools) = tools_opt {
                serde_json::json!({
                    "messages": msgs,
                    "tools": tools,
                    "tool_choice": "auto",
                    "max_tokens": 2048,
                    "temperature": 0.3,
                    "stream": true,
                })
            } else {
                serde_json::json!({
                    "messages": msgs,
                    "max_tokens": 2048,
                    "temperature": 0.3,
                    "stream": true,
                })
            };
            let result = send_streaming_request(&client, &base, body, &app, Some(cancel2))
                .await.map_err(|e| e.to_string())?;
            let tool_calls = detect_tool_calls(&result);
            Ok(LlmRound { full_text: result.full_text, tool_calls })
        })
    });

    let registry = build_memory_registry(
        state.db.clone(),
        vault_id,
        vault_path.clone(),
    );

    let emit: crate::runtime::types::EmitEventFn = Arc::new(|_, _| {});  // 靜默，不 emit 前端事件

    let tools_json = serde_json::Value::Array(memory_agent_tools());
    let session_id = uuid::Uuid::new_v4().to_string();

    internal_write_log(&vault_path, "INFO",
        &format!("記憶 sub-agent 啟動 session={}", session_id)).await;

    let _ = run_sub_agent(
        session_id,
        "scheduler".to_string(),
        "custom",
        "請開始分析並提取記憶。",
        SYSTEM_PROMPT,
        tools_json,
        registry,
        llm_fn,
        emit,
    ).await;
}

