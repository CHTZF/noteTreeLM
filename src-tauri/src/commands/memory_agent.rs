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
    vault_id: &str,
    limit: i64,
    client: &reqwest::Client,
    auth_token: Option<&str>,
) -> Vec<ConversationPreview> {
    use crate::api_client::daemon_get;
    let path = format!("/conversations?vault_id={}", urlencoding::encode(vault_id));
    let result: serde_json::Value = match daemon_get(client, &path, auth_token).await {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let arr = match result.as_array() {
        Some(a) => a.clone(),
        None => return vec![],
    };
    arr.iter()
        .filter(|c| c["memory_processed"].as_bool() != Some(true))
        .take(limit as usize)
        .filter_map(|c| {
            let raw_id = c["id"].as_str().unwrap_or("").to_string();
            let id = raw_id
                .strip_prefix("conversations:")
                .unwrap_or(&raw_id)
                .to_string();
            if id.is_empty() { return None; }
            let title = c["title"].as_str().unwrap_or("未知對話").to_string();
            let updated_at = c["updated_at"].as_i64().unwrap_or(0);
            let messages_str = c["messages_json"].as_str().unwrap_or("[]");
            let msgs: serde_json::Value =
                serde_json::from_str(messages_str).unwrap_or_default();
            let message_count =
                msgs.as_array().map(|a| a.len() as i64).unwrap_or(0);
            let preview = msgs
                .as_array()
                .and_then(|a| a.iter().rev().find(|m| m["role"].as_str() == Some("user")))
                .and_then(|m| m["content"].as_str())
                .map(|s| s.chars().take(100).collect::<String>())
                .unwrap_or_default();
            Some(ConversationPreview { id, title, message_count, preview, updated_at })
        })
        .collect()
}

pub(crate) async fn internal_get_content(
    conversation_id: &str,
    client: &reqwest::Client,
    auth_token: Option<&str>,
) -> String {
    use crate::api_client::daemon_get;
    let path = format!("/conversations/{}/messages", urlencoding::encode(conversation_id));
    let result: serde_json::Value = match daemon_get(client, &path, auth_token).await {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let messages_str = result["messages_json"].as_str().unwrap_or("[]");
    let msgs: serde_json::Value =
        serde_json::from_str(messages_str).unwrap_or_default();
    let arr = match msgs.as_array() {
        Some(a) => a,
        None => return String::new(),
    };
    let mut lines = Vec::new();
    for msg in arr {
        let role = msg["role"].as_str().unwrap_or("unknown");
        let content = msg["content"].as_str().unwrap_or("");
        if !content.is_empty() && (role == "user" || role == "assistant") {
            lines.push(format!("[{}]: {}", role, content));
        }
    }
    lines.join("\n\n")
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
    conversation_id: &str,
    client: &reqwest::Client,
    auth_token: Option<&str>,
) {
    use crate::api_client::daemon_patch;
    let path = format!("/conversations/{}/processed", urlencoding::encode(conversation_id));
    let _ = daemon_patch::<_, serde_json::Value>(
        client,
        &path,
        &serde_json::json!({}),
        auth_token,
    )
    .await;
}

// ─── Tauri Commands（前端手動呼叫） ──────────────────────────────────────────

#[tauri::command]
pub async fn get_unprocessed_conversations(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> Result<Vec<ConversationPreview>, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(vec![]); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    Ok(internal_get_unprocessed(
        &vault_id,
        limit.unwrap_or(20) as i64,
        &state.http_client,
        tok,
    ).await)
}

#[tauri::command]
pub async fn get_conversation_content(
    state: State<'_, AppState>,
    conversation_id: String,
) -> Result<String, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    Ok(internal_get_content(&conversation_id, &state.http_client, tok).await)
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
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    internal_mark_processed(&conversation_id, &state.http_client, tok).await;
    Ok(())
}

/// 手動觸發記憶 sub-agent（前端設定頁按鈕）
#[tauri::command]
pub async fn trigger_memory_agent(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), AppError> {
    let vault_uuid = state.get_vault_uuid().await;
    let vault_path = state.get_vault_path().await;
    if vault_uuid.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault".to_string()));
    }
    tokio::spawn(async move {
        run_memory_agent_loop(&app, vault_uuid, vault_path).await;
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
    vault_id: String,
    vault_path: String,
    client: reqwest::Client,
    auth_token: String,
) -> Arc<crate::runtime::tool_registry::ToolRegistry> {
    use crate::runtime::tool_registry::ToolRegistry;
    use crate::runtime::types::Tool;

    let mut registry = ToolRegistry::new();

    // get_unprocessed_conversations
    {
        let vid = vault_id.clone();
        let c = client.clone();
        let tok = auth_token.clone();
        registry.register("get_unprocessed_conversations".into(), Tool {
            execute: Arc::new(move |args| {
                let vid = vid.clone();
                let c = c.clone();
                let tok = tok.clone();
                let limit = args["limit"].as_i64().unwrap_or(20);
                Box::pin(async move {
                    let tok_ref: Option<&str> = if tok.is_empty() { None } else { Some(&tok) };
                    let list = internal_get_unprocessed(&vid, limit, &c, tok_ref).await;
                    Ok(serde_json::to_value(list).unwrap_or(serde_json::json!([])))
                })
            }),
            rollback: None,
        });
    }

    // get_conversation_content
    {
        let c = client.clone();
        let tok = auth_token.clone();
        registry.register("get_conversation_content".into(), Tool {
            execute: Arc::new(move |args| {
                let c = c.clone();
                let tok = tok.clone();
                let cid = args["conversation_id"].as_str().unwrap_or("").to_string();
                Box::pin(async move {
                    let tok_ref: Option<&str> = if tok.is_empty() { None } else { Some(&tok) };
                    let text = internal_get_content(&cid, &c, tok_ref).await;
                    Ok(serde_json::Value::String(text))
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
        let c = client.clone();
        let tok = auth_token.clone();
        registry.register("mark_conversation_processed".into(), Tool {
            execute: Arc::new(move |args| {
                let c = c.clone();
                let tok = tok.clone();
                let cid = args["conversation_id"].as_str().unwrap_or("").to_string();
                Box::pin(async move {
                    let tok_ref: Option<&str> = if tok.is_empty() { None } else { Some(&tok) };
                    internal_mark_processed(&cid, &c, tok_ref).await;
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
#[allow(dead_code)]
pub async fn run_scheduled_agent(
    app: &tauri::AppHandle,
    vault_id: String,
    agent_type: Option<String>,
    agent_prompt: Option<String>,
    description: String,
) {
    use tauri::Manager;
    match agent_type.as_deref() {
        Some("memory_agent") => {
            // vault_path: use the state's current vault path (DB migrated to daemon)
            let vault_path = {
                let state = app.state::<AppState>();
                state.get_vault_path().await
            };
            if vault_path.is_empty() {
                eprintln!("[scheduler] vault not found for vault_id={}", vault_id);
                return;
            }
            run_memory_agent_loop(app, vault_id, vault_path).await;
        }
        Some(unknown_type) => {
            // 未知 agent_type：用 vault registry 跑通用 sub-agent
            let state = app.state::<AppState>();
            // 使用任務指定的 vault_id，而非 active vault
            let vault_path = vault_id.clone();
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

            let ma_auth_token = state.get_auth_token().await;
            let registry = crate::tools::build_vault_registry(
                vault_path.clone(),
                vault_id.clone(),
                state.http_client.clone(),
                ma_auth_token,
                app.clone(),
                Some(base_url_for_registry),
                crate::tools::make_late_llm_fn(),
                Arc::new(tokio::sync::Mutex::new(None)),
                Arc::clone(&state.system_agent),
                Some(cancel),
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

pub async fn run_memory_agent_loop(app: &tauri::AppHandle, vault_uuid: String, vault_path: String) {
    use crate::commands::ai::{send_streaming_request, detect_tool_calls};
    use crate::runtime::sub_agent::run_sub_agent;
    use crate::runtime::types::LlmRound;
    use tauri::Manager;

    if vault_path.is_empty() || vault_uuid.is_empty() {
        return;
    }

    let state = app.state::<AppState>();

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

    let auth_token = state.get_auth_token().await;
    let registry = build_memory_registry(
        vault_uuid,
        vault_path.clone(),
        state.http_client.clone(),
        auth_token,
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

