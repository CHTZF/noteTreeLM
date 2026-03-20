/// tools/mod.rs
/// 將現有 vault 工具函數包裝成 runtime ToolRegistry 格式。
///
/// 每個工具對應一個 `Tool { execute, rollback }`：
/// - 唯讀工具（list_structure / read_note / search_vault / query_memory）：rollback = None
/// - 寫入工具（create_note / update_note / create_folder）：rollback 實作還原邏輯
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex;

use crate::commands::ai::{
    resolve_vault_path, tool_create_folder, tool_create_note, tool_list_structure,
    tool_read_note, tool_search_vault, tool_update_note, call_external_ai_via_db,
    tool_list_recent_conversations, tool_create_agent_skill, vault_tools,
};
use crate::commands::knowledge_import::tool_web_search;
use crate::db::surreal::SurrealDb;
use crate::runtime::memory_agent::{add_memory_rule_to_db, tool_query_memory};
use std::sync::atomic::AtomicBool;
use crate::runtime::system_agent::{AgentRequest, NewSkillSpec, SystemAgentService};
use crate::runtime::tool_registry::ToolRegistry;
use crate::runtime::types::{ConfirmWriteFn, LlmFn, Tool};

/// 建立包含所有 vault 工具的 ToolRegistry。
///
/// # 參數
/// - `vault_path`:    Vault 根目錄絕對路徑
/// - `vault_id`:      Vault ID
/// - `vault_db`:      SurrealDB 連線
/// - `app`:           Tauri AppHandle
/// - `emb_url`:       Embedding server URL（可選）
/// - `search_method_tx`: 搜尋方式選擇 channel
/// - `llm_fn_late`:   延遲繫結的 LlmFn（invoke_agent 在建完後設定）
/// - `registry_late`: 延遲繫結的 Arc<ToolRegistry>（Arc::new(registry) 後設定）
pub fn build_vault_registry(
    vault_path: String,
    vault_db: SurrealDb,
    vault_id: String,
    app: AppHandle,
    emb_url: Option<String>,
    search_method_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    llm_fn_late: Arc<Mutex<Option<LlmFn>>>,
    registry_late: Arc<Mutex<Option<Arc<ToolRegistry>>>>,
    system_agent_svc: Arc<SystemAgentService>,
    cancel: Option<Arc<AtomicBool>>,
    api_key_cache: Arc<Mutex<HashMap<String, String>>>,
) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();

    // ── 唯讀工具 ──────────────────────────────────────────────────

    // list_structure
    {
        let vp = vault_path.clone();
        registry.register(
            "list_structure".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let vp = vp.clone();
                    Box::pin(async move { Ok(Value::String(tool_list_structure(&path, &vp))) })
                }),
                rollback: None,
            },
        );
    }

    // read_note
    {
        let vp = vault_path.clone();
        registry.register(
            "read_note".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let vp = vp.clone();
                    Box::pin(async move { Ok(Value::String(tool_read_note(&path, &vp))) })
                }),
                rollback: None,
            },
        );
    }

    // search_vault
    {
        let _vp = vault_path.clone();
        let db = vault_db.clone();
        let vid = vault_id.clone();
        let app = app.clone();
        registry.register(
            "search_vault".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let query = args["query"].as_str().unwrap_or("").to_string();
                    let db = db.clone();
                    let vid = vid.clone();
                    let app = app.clone();
                    Box::pin(async move {
                        Ok(Value::String(tool_search_vault(&query, &db, &vid, &app).await))
                    })
                }),
                rollback: None,
            },
        );
    }

    // query_memory
    {
        let db = vault_db.clone();
        let vid = vault_id.clone();
        let emb = emb_url.clone();
        registry.register(
            "query_memory".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let keywords: Vec<String> = args["keywords"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let since = args["since"].as_str().map(String::from);
                    let limit = args["limit"].as_u64().map(|v| v as usize);
                    let db = db.clone();
                    let vid = vid.clone();
                    let emb = emb.clone();
                    Box::pin(async move {
                        Ok(Value::String(tool_query_memory(keywords, since, limit, &db, &vid, emb.as_deref()).await))
                    })
                }),
                rollback: None,
            },
        );
    }

    // ── 寫入工具（含 rollback）────────────────────────────────────

    // create_note — rollback: 刪除剛建立的檔案
    {
        let vp_exec = vault_path.clone();
        let vp_rb = vault_path.clone();
        let db_cn = vault_db.clone();
        let vid_cn = vault_id.clone();
        registry.register(
            "create_note".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let content = args["content"].as_str().unwrap_or("").to_string();
                    let vp = vp_exec.clone();
                    let db = db_cn.clone();
                    let vid = vid_cn.clone();
                    Box::pin(async move {
                        let result = tool_create_note(&path, &content, &vp, Some((db, vid))).await;
                        if result.contains("失敗") {
                            Err(result)
                        } else {
                            Ok(Value::String(result))
                        }
                    })
                }),
                rollback: Some(Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let vp = vp_rb.clone();
                    Box::pin(async move {
                        if let Ok(abs_path) = resolve_vault_path(&path, &vp) {
                            let _ = tokio::fs::remove_file(&abs_path).await;
                        }
                        Ok(Value::Null)
                    })
                })),
            },
        );
    }

    // update_note — rollback: 還原原始內容
    // 使用 Arc<Mutex<HashMap<path, backup>>> 讓同一個 tx 中多次呼叫各自備份
    {
        let vp_exec = vault_path.clone();
        let vp_rb = vault_path.clone();
        let db_un = vault_db.clone();
        let vid_un = vault_id.clone();
        let backups: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let backups_rb = Arc::clone(&backups);

        registry.register(
            "update_note".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let content = args["content"].as_str().unwrap_or("").to_string();
                    let vp = vp_exec.clone();
                    let db = db_un.clone();
                    let vid = vid_un.clone();
                    let backups = Arc::clone(&backups);
                    Box::pin(async move {
                        let abs_path = resolve_vault_path(&path, &vp).map_err(|e| e)?;
                        // 先備份原始內容
                        let original =
                            tokio::fs::read_to_string(&abs_path).await.unwrap_or_default();
                        backups.lock().await.insert(path.clone(), original);
                        // 寫入新內容並同步 DB
                        let result = tool_update_note(&path, &content, &vp, Some((db, vid))).await;
                        if result.contains("失敗") {
                            return Err(result);
                        }
                        Ok(Value::String(result))
                    })
                }),
                rollback: Some(Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let vp = vp_rb.clone();
                    let backups = Arc::clone(&backups_rb);
                    Box::pin(async move {
                        let abs_path = resolve_vault_path(&path, &vp).map_err(|e| e)?;
                        if let Some(original) = backups.lock().await.remove(&path) {
                            tokio::fs::write(&abs_path, original)
                                .await
                                .map_err(|e| format!("還原失敗：{}", e))?;
                        }
                        Ok(Value::Null)
                    })
                })),
            },
        );
    }

    // create_folder — rollback: 移除剛建立的資料夾（僅在空的時候才會成功，符合安全預期）
    {
        let vp_exec = vault_path.clone();
        let vp_rb = vault_path.clone();
        registry.register(
            "create_folder".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let vp = vp_exec.clone();
                    Box::pin(async move {
                        let result = tool_create_folder(&path, &vp).await;
                        if result.contains("失敗") {
                            Err(result)
                        } else {
                            Ok(Value::String(result))
                        }
                    })
                }),
                rollback: Some(Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let vp = vp_rb.clone();
                    Box::pin(async move {
                        if let Ok(abs_path) = resolve_vault_path(&path, &vp) {
                            // remove_dir 只能移除空資料夾，非空則安全失敗
                            let _ = tokio::fs::remove_dir(&abs_path).await;
                        }
                        Ok(Value::Null)
                    })
                })),
            },
        );
    }

    // add_memory_rule — 讓 LLM 學習新的時間表達式規則（冪等，rollback 不需要）
    {
        let db = vault_db.clone();
        let vid = vault_id.clone();
        registry.register(
            "add_memory_rule".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let ptype   = args["pattern_type"].as_str().unwrap_or("").to_string();
                    let pattern = args["pattern"].as_str().unwrap_or("").to_string();
                    let value   = args["value"].as_str().unwrap_or("").to_string();
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        Ok(Value::String(add_memory_rule_to_db(&db, &vid, &ptype, &pattern, &value).await))
                    })
                }),
                rollback: None,
            },
        );
    }

    // open_note — 發送 ui:open_note 事件讓前端開啟筆記（唯讀，無 rollback）
    // 驗證路徑：先試 vault_path/path，找不到時掃描同名檔，前端接收 absolute path
    {
        let app = app.clone();
        let vp = vault_path.clone();
        registry.register(
            "open_note".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let app = app.clone();
                    let vp = vp.clone();
                    let mut rel = args["path"].as_str().unwrap_or("").to_string();
                    if !rel.is_empty() && !rel.ends_with(".md") {
                        rel.push_str(".md");
                    }
                    Box::pin(async move {
                        if rel.is_empty() {
                            return Err("請提供筆記路徑".to_string());
                        }
                        // 1. 先試完整 vault_path/rel
                        let abs = std::path::PathBuf::from(&vp).join(&rel);
                        if abs.exists() {
                            let abs_str = abs.to_string_lossy().to_string();
                            let _ = app.emit("ui:open_note", &abs_str);
                            return Ok(Value::String(format!("✅ 已打開筆記：{}", rel)));
                        }
                        // 2. 嘗試只比對檔名（rel 可能缺少資料夾前綴）
                        let filename = std::path::Path::new(&rel)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| rel.clone());
                        let mut found: Option<String> = None;
                        if let Ok(walker) = std::fs::read_dir(&vp) {
                            // 只搜一層 vault root（常見情形）
                            for entry in walker.flatten() {
                                if entry.file_name().to_string_lossy() == filename {
                                    found = Some(entry.path().to_string_lossy().to_string());
                                    break;
                                }
                            }
                        }
                        // 3. 若還找不到，遞迴 walk 整個 vault（深層搜尋）
                        if found.is_none() {
                            fn find_in_dir(dir: &std::path::Path, name: &str, depth: u32) -> Option<String> {
                                if depth > 6 { return None; }
                                let Ok(entries) = std::fs::read_dir(dir) else { return None; };
                                for entry in entries.flatten() {
                                    let p = entry.path();
                                    if p.file_name().map(|n| n.to_string_lossy().as_ref() == name).unwrap_or(false) {
                                        return Some(p.to_string_lossy().to_string());
                                    }
                                    if p.is_dir() {
                                        if let Some(r) = find_in_dir(&p, name, depth + 1) {
                                            return Some(r);
                                        }
                                    }
                                }
                                None
                            }
                            found = find_in_dir(std::path::Path::new(&vp), &filename, 1);
                        }
                        match found {
                            Some(abs_str) => {
                                let _ = app.emit("ui:open_note", &abs_str);
                                Ok(Value::String(format!("✅ 已打開筆記：{}", filename)))
                            }
                            None => Err(format!(
                                "找不到筆記「{}」，請先用 search_vault 確認正確路徑後再呼叫 open_note",
                                rel
                            )),
                        }
                    })
                }),
                rollback: None,
            },
        );
    }

    // call_external_ai — 呼叫前暫停，等待前端選擇 web_search 或 call_external_ai
    {
        let db = vault_db.clone();
        let vid = vault_id.clone();
        let app = app.clone();
        let emb = emb_url.clone();
        let tx = search_method_tx.clone();
        let cache = Arc::clone(&api_key_cache);
        registry.register(
            "call_external_ai".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let query = args["query"].as_str().unwrap_or("").to_string();
                    let db = db.clone();
                    let vid = vid.clone();
                    let app = app.clone();
                    let emb = emb.clone();
                    let tx = tx.clone();
                    let cache = Arc::clone(&cache);
                    Box::pin(async move {
                        // 通知前端選擇搜尋方式
                        let _ = app.emit("agent:search_method_request", serde_json::json!({
                            "query": query
                        }));

                        // 等待前端回覆（60s timeout → 預設 call_external_ai）
                        let method = {
                            let (ch_tx, ch_rx) = tokio::sync::oneshot::channel::<String>();
                            *tx.lock().await = Some(ch_tx);
                            tokio::time::timeout(
                                std::time::Duration::from_secs(60),
                                ch_rx,
                            )
                            .await
                            .unwrap_or(Ok("call_external_ai".to_string()))
                            .unwrap_or_else(|_| "call_external_ai".to_string())
                        };

                        let result = if method == "web_search" {
                            tool_web_search(&db, &vid, &query, &app, emb.as_deref()).await
                        } else {
                            // Emit synthetic web_refs so frontend can show "儲存為知識"
                            let _ = app.emit("agent:web_refs", serde_json::json!([
                                {"path": "", "title": query, "excerpt": ""}
                            ]));
                            call_external_ai_via_db(&query, &db, &app, &cache).await
                        };
                        Ok(Value::String(result))
                    })
                }),
                rollback: None,
            },
        );
    }

    // web_search — 搜尋 DuckDuckGo Lite，結果自動背景匯入知識庫（唯讀，無 rollback）
    {
        let db = vault_db.clone();
        let vid = vault_id.clone();
        let app = app.clone();
        let emb = emb_url.clone();
        registry.register(
            "web_search".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let query = args["query"].as_str().unwrap_or("").to_string();
                    let db = db.clone();
                    let vid = vid.clone();
                    let app = app.clone();
                    let emb = emb.clone();
                    Box::pin(async move {
                        Ok(Value::String(
                            tool_web_search(&db, &vid, &query, &app, emb.as_deref()).await,
                        ))
                    })
                }),
                rollback: None,
            },
        );
    }

    // list_recent_conversations — 讀取最近對話記錄，供 reflection agent 分析
    {
        let db = vault_db.clone();
        let vid = vault_id.clone();
        registry.register(
            "list_recent_conversations".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let limit = args["limit"].as_u64().unwrap_or(10) as usize;
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        Ok(Value::String(tool_list_recent_conversations(&db, &vid, limit).await))
                    })
                }),
                rollback: None,
            },
        );
    }

    // create_agent_skill — reflection agent 建立新技能規範（預設未啟用）
    {
        let db = vault_db.clone();
        let vid = vault_id.clone();
        let emb = emb_url.clone();
        registry.register(
            "create_agent_skill".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let title          = args["title"].as_str().unwrap_or("").to_string();
                    let trigger        = args["trigger"].as_str().unwrap_or("").to_string();
                    let behavior       = args["behavior"].as_str().unwrap_or("").to_string();
                    let injection_mode = args["injection_mode"].as_str().unwrap_or("passive").to_string();
                    let db  = db.clone();
                    let vid = vid.clone();
                    let emb = emb.clone();
                    Box::pin(async move {
                        Ok(Value::String(
                            tool_create_agent_skill(&db, &vid, &title, &trigger, &behavior, &injection_mode, emb.as_deref()).await
                        ))
                    })
                }),
                rollback: None,
            },
        );
    }

    // call_agent — 透過 SystemAgentService 路由，委派任務給對應的 agent definition
    {
        let app_ca   = app.clone();
        let llm_late = Arc::clone(&llm_fn_late);
        let reg_late = Arc::clone(&registry_late);
        let svc      = Arc::clone(&system_agent_svc);
        let cancel_ca = cancel.clone();
        let emb_ca_call = emb_url.clone();

        registry.register(
            "call_agent".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let target  = args["target"].as_str().unwrap_or("search").to_string();
                    let task    = args["task"].as_str().unwrap_or("").to_string();
                    let context = args["context"].as_str().unwrap_or("").to_string();
                    let llm_late = Arc::clone(&llm_late);
                    let reg_late = Arc::clone(&reg_late);
                    let svc     = Arc::clone(&svc);
                    let app     = app_ca.clone();
                    let cancel  = cancel_ca.clone();
                    let emb     = emb_ca_call.clone();
                    Box::pin(async move {
                        let llm_fn = {
                            let guard = llm_late.lock().await;
                            match guard.clone() {
                                Some(f) => f,
                                None => return Err("call_agent: llm_fn 尚未初始化".to_string()),
                            }
                        };
                        let sub_registry = {
                            let guard = reg_late.lock().await;
                            match guard.clone() {
                                Some(r) => r,
                                None => return Err("call_agent: registry 尚未初始化".to_string()),
                            }
                        };
                        let emit: crate::runtime::types::EmitEventFn = {
                            let app = app.clone();
                            Arc::new(move |event: String, payload: Value| {
                                let _ = tauri::Emitter::emit(&app, &event, payload);
                            })
                        };
                        let app_state = app.state::<crate::state::AppState>();
                        let parent_system = String::new();
                        let confirm_write: ConfirmWriteFn = {
                            let write_tx: Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<bool>>>> = Arc::clone(&app_state.write_confirm_tx);
                            Arc::new(move |_display: String| {
                                let tx = Arc::clone(&write_tx);
                                Box::pin(async move {
                                    let (ch_tx, ch_rx) = tokio::sync::oneshot::channel::<bool>();
                                    *tx.lock().await = Some(ch_tx);
                                    tokio::time::timeout(std::time::Duration::from_secs(60), ch_rx)
                                        .await.unwrap_or(Ok(false)).unwrap_or(false)
                                })
                            })
                        };
                        let result = svc.route(
                            AgentRequest {
                                caller_session_id: String::new(),
                                target,
                                task,
                                context,
                                parent_system,
                                conversation_id: None,
                                vault_path: String::new(),
                            },
                            vault_tools(),
                            sub_registry,
                            llm_fn,
                            emit,
                            cancel,
                            emb.as_deref(),
                            confirm_write,
                            None,
                        ).await;
                        Ok(Value::String(result))
                    })
                }),
                rollback: None,
            },
        );
    }

    // touch_agent — Chat agent 使用：自動語意匹配現有 agent 或建立新 agent，然後執行
    {
        let app_ta   = app.clone();
        let llm_late_ta = Arc::clone(&llm_fn_late);
        let reg_late_ta = Arc::clone(&registry_late);
        let svc_ta   = Arc::clone(&system_agent_svc);
        let cancel_ta = cancel.clone();
        let emb_ta   = emb_url.clone();

        registry.register(
            "touch_agent".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let task    = args["task"].as_str().unwrap_or("").to_string();
                    let name    = args["name"].as_str()
                        .map(String::from)
                        .unwrap_or_else(|| task.chars().take(20).collect());
                    let description = args["description"].as_str().unwrap_or("").to_string();
                    let trigger = args["trigger"].as_str()
                        .map(String::from)
                        .unwrap_or_else(|| task.clone());
                    let context = args["context"].as_str().unwrap_or("").to_string();
                    let tool_names: Vec<String> = args["tool_names"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    let llm_late = Arc::clone(&llm_late_ta);
                    let reg_late = Arc::clone(&reg_late_ta);
                    let svc  = Arc::clone(&svc_ta);
                    let app  = app_ta.clone();
                    let cancel = cancel_ta.clone();
                    let emb  = emb_ta.clone();
                    Box::pin(async move {
                        if task.is_empty() {
                            return Err("touch_agent: task 必填".to_string());
                        }
                        let llm_fn = {
                            let guard = llm_late.lock().await;
                            match guard.clone() {
                                Some(f) => f,
                                None => return Err("touch_agent: llm_fn 尚未初始化".to_string()),
                            }
                        };
                        let sub_registry = {
                            let guard = reg_late.lock().await;
                            match guard.clone() {
                                Some(r) => r,
                                None => return Err("touch_agent: registry 尚未初始化".to_string()),
                            }
                        };
                        let emit: crate::runtime::types::EmitEventFn = {
                            let app = app.clone();
                            Arc::new(move |event: String, payload: Value| {
                                let _ = tauri::Emitter::emit(&app, &event, payload);
                            })
                        };
                        // Step 1：touch_agent METHOD — 用 task 語意找或建立 AgentDefinition
                        let def = svc.touch_agent(
                            name.clone(), description, trigger,
                            tool_names, String::new(), vec![], 5,
                            emb.as_deref(), emit.clone(), &task,
                        ).await;

                        // Step 2：route — 用找到/建立的 def_id 執行 sub-agent
                        let target = match def {
                            Ok(ref d) => d.def_id.clone(),
                            Err(_) => name, // fallback to hint name
                        };
                        let app_state2 = app.state::<crate::state::AppState>();
                        let parent_system2 = String::new();
                        let confirm_write: ConfirmWriteFn = {
                            let write_tx: Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<bool>>>> = Arc::clone(&app_state2.write_confirm_tx);
                            Arc::new(move |_display: String| {
                                let tx = Arc::clone(&write_tx);
                                Box::pin(async move {
                                    let (ch_tx, ch_rx) = tokio::sync::oneshot::channel::<bool>();
                                    *tx.lock().await = Some(ch_tx);
                                    tokio::time::timeout(std::time::Duration::from_secs(60), ch_rx)
                                        .await.unwrap_or(Ok(false)).unwrap_or(false)
                                })
                            })
                        };
                        let result = svc.route(
                            crate::runtime::system_agent::AgentRequest {
                                caller_session_id: String::new(),
                                target,
                                task,
                                context,
                                parent_system: parent_system2,
                                conversation_id: None,
                                vault_path: String::new(),
                            },
                            crate::commands::ai::vault_tools(),
                            sub_registry,
                            llm_fn,
                            emit,
                            cancel,
                            emb.as_deref(),
                            confirm_write,
                            None,
                        ).await;
                        Ok(Value::String(result))
                    })
                }),
                rollback: None,
            },
        );
    }

    // create_agent — 透過 SystemAgentService 動態建立 agent definition + skills
    {
        let svc_ca = Arc::clone(&system_agent_svc);
        let emb_ca = emb_url.clone();
        let app_ca = app.clone();

        registry.register(
            "create_agent".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let svc = Arc::clone(&svc_ca);
                    let emb = emb_ca.clone();
                    let app = app_ca.clone();
                    Box::pin(async move {
                        let name        = args["name"].as_str().unwrap_or("").to_string();
                        let description = args["description"].as_str().unwrap_or("").to_string();
                        let trigger     = args["trigger"].as_str().unwrap_or("").to_string();
                        let max_rounds  = args["max_rounds"].as_i64().unwrap_or(5);

                        let _ = tauri::Emitter::emit(&app, "agent:pre_route_debug", serde_json::json!({
                            "step": "create_agent_called",
                            "name": name,
                            "trigger": trigger,
                            "emb_url": emb.as_deref().unwrap_or("(none)"),
                        }));

                        if name.is_empty() {
                            return Err("create_agent: name 必填".to_string());
                        }

                        let tool_names: Vec<String> = args["tool_names"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default();

                        let system_prompt = args["system_prompt"].as_str().unwrap_or("").to_string();

                        let skills: Vec<NewSkillSpec> = args["skills"]
                            .as_array()
                            .map(|a| {
                                a.iter().filter_map(|v| {
                                    serde_json::from_value::<NewSkillSpec>(v.clone()).ok()
                                }).collect()
                            })
                            .unwrap_or_default();

                        let emit: crate::runtime::types::EmitEventFn = {
                            let app = app.clone();
                            Arc::new(move |event: String, payload: Value| {
                                let _ = tauri::Emitter::emit(&app, &event, payload);
                            })
                        };

                        match svc.create_agent(
                            name.clone(), description, trigger, tool_names, system_prompt,
                            skills, max_rounds, emb.as_deref(), emit,
                        ).await {
                            Ok(def) => Ok(Value::String(format!(
                                "Agent「{}」就緒 (def_id: {})，共 {} 個 skills。請立即使用 call_agent 執行任務。",
                                def.name, def.def_id, def.skill_ids.len()
                            ))),
                            Err(e) => Err(format!("create_agent 失敗: {e}")),
                        }
                    })
                }),
                rollback: None,
            },
        );
    }

    // list_available_agents — 查詢 DB 中所有 active agent definitions
    {
        let db_la    = vault_db.clone();
        let vid_la   = vault_id.clone();

        registry.register(
            "list_available_agents".into(),
            Tool {
                execute: Arc::new(move |_args: Value| {
                    let db  = db_la.clone();
                    let vid = vid_la.clone();
                    Box::pin(async move {
                        #[derive(serde::Deserialize)]
                        struct DefSummary {
                            def_id: String,
                            name: String,
                            description: String,
                            kind: String,
                            tool_names: Vec<String>,
                            #[allow(dead_code)]
                            created_at: surrealdb::sql::Datetime,
                        }
                        let mut resp = db.query(
                            "SELECT def_id, name, description, kind, tool_names, created_at \
                             FROM agent_definitions \
                             WHERE vault_id = $vid AND is_active = true \
                             ORDER BY created_at ASC"
                        )
                        .bind(("vid", vid.clone()))
                        .await
                        .map_err(|e| e.to_string())?;

                        let rows: Vec<DefSummary> = resp.take(0).unwrap_or_default();

                        let lines: Vec<String> = rows.iter().map(|d| {
                            format!(
                                "- **{}** (id: `{}`, kind: {}) — {} [tools: {}]",
                                d.name,
                                d.def_id,
                                d.kind,
                                d.description,
                                d.tool_names.join(", ")
                            )
                        }).collect();

                        if lines.is_empty() {
                            Ok(Value::String("目前沒有可用的 agent definitions。".to_string()))
                        } else {
                            Ok(Value::String(format!("# 可用 Agents\n{}", lines.join("\n"))))
                        }
                    })
                }),
                rollback: None,
            },
        );
    }

    // ── 新增工具 ──────────────────────────────────────────────────

    // get_current_datetime — 唯讀，回傳本地時間字串
    {
        registry.register(
            "get_current_datetime".into(),
            Tool {
                execute: Arc::new(move |_args: Value| {
                    Box::pin(async move {
                        let now = chrono::Local::now();
                        Ok(Value::String(format!("{}", now.format("%Y-%m-%d %H:%M:%S %z"))))
                    })
                }),
                rollback: None,
            },
        );
    }

    // list_notes_in_folder — 唯讀，列出資料夾內的筆記
    {
        let db = vault_db.clone();
        let vid = vault_id.clone();
        registry.register(
            "list_notes_in_folder".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let folder = args["folder"].as_str().unwrap_or("").to_string();
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        if folder.is_empty() {
                            return Err("請提供資料夾路徑".to_string());
                        }
                        // 確保前綴以 / 結尾進行 starts_with 匹配
                        let prefix = if folder.ends_with('/') {
                            folder.clone()
                        } else {
                            format!("{}/", folder)
                        };

                        #[derive(serde::Deserialize)]
                        struct NoteRow {
                            title: Option<String>,
                            path: String,
                        }

                        let mut resp = db.query(
                            "SELECT title, path FROM notes \
                             WHERE vault_id = $vid AND (path = $exact OR string::starts_with(path, $prefix)) \
                             ORDER BY path ASC LIMIT 200"
                        )
                        .bind(("vid", vid.clone()))
                        .bind(("exact", format!("{}.md", folder)))
                        .bind(("prefix", prefix.clone()))
                        .await
                        .map_err(|e| e.to_string())?;

                        let rows: Vec<NoteRow> = resp.take(0).unwrap_or_default();

                        if rows.is_empty() {
                            return Ok(Value::String(format!("資料夾「{}」中沒有筆記。", folder)));
                        }

                        let lines: Vec<String> = rows.iter().map(|r| {
                            let title = r.title.as_deref().unwrap_or("(無標題)");
                            format!("- {} ({})", title, r.path)
                        }).collect();

                        Ok(Value::String(format!(
                            "資料夾「{}」共 {} 篇筆記：\n{}",
                            folder, lines.len(), lines.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // append_to_note — 寫入工具，在既有筆記末尾追加內容
    {
        let vp = vault_path.clone();
        let app_an = app.clone();
        registry.register(
            "append_to_note".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let content = args["content"].as_str().unwrap_or("").to_string();
                    let vp = vp.clone();
                    let app = app_an.clone();
                    Box::pin(async move {
                        if path.is_empty() {
                            return Err("請提供筆記路徑".to_string());
                        }
                        let rel = if path.ends_with(".md") { path.clone() } else { format!("{}.md", path) };
                        let abs = std::path::PathBuf::from(&vp).join(&rel);
                        if !abs.exists() {
                            return Err(format!("找不到筆記：{}", rel));
                        }
                        let existing = tokio::fs::read_to_string(&abs).await
                            .map_err(|e| format!("讀取失敗：{}", e))?;
                        let new_content = format!("{}\n{}", existing.trim_end(), content);
                        tokio::fs::write(&abs, &new_content).await
                            .map_err(|e| format!("寫入失敗：{}", e))?;
                        let abs_str = abs.to_string_lossy().to_string();
                        let _ = app.emit("ui:open_note", &abs_str);
                        Ok(Value::String(format!("已追加內容至 {}", rel)))
                    })
                }),
                rollback: None,
            },
        );
    }

    // delete_note — 寫入工具（需確認）
    {
        let vp = vault_path.clone();
        let db = vault_db.clone();
        let vid = vault_id.clone();
        registry.register(
            "delete_note".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let vp = vp.clone();
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        if path.is_empty() {
                            return Err("請提供筆記路徑".to_string());
                        }
                        let rel = if path.ends_with(".md") { path.clone() } else { format!("{}.md", path) };
                        // 1. 先試完整路徑
                        let direct = std::path::PathBuf::from(&vp).join(&rel);
                        let abs_path = if direct.exists() {
                            direct.to_string_lossy().to_string()
                        } else {
                            // 2. 只比對檔名（root scan）
                            let filename = std::path::Path::new(&rel)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| rel.clone());
                            let mut found: Option<String> = None;
                            if let Ok(entries) = std::fs::read_dir(&vp) {
                                for entry in entries.flatten() {
                                    if entry.file_name().to_string_lossy() == filename {
                                        found = Some(entry.path().to_string_lossy().to_string());
                                        break;
                                    }
                                }
                            }
                            // 3. 遞迴搜尋
                            if found.is_none() {
                                fn find_in_dir(dir: &std::path::Path, name: &str, depth: u32) -> Option<String> {
                                    if depth > 6 { return None; }
                                    let Ok(entries) = std::fs::read_dir(dir) else { return None; };
                                    for entry in entries.flatten() {
                                        let p = entry.path();
                                        if p.file_name().map(|n| n.to_string_lossy().as_ref() == name).unwrap_or(false) {
                                            return Some(p.to_string_lossy().to_string());
                                        }
                                        if p.is_dir() {
                                            if let Some(r) = find_in_dir(&p, name, depth + 1) {
                                                return Some(r);
                                            }
                                        }
                                    }
                                    None
                                }
                                found = find_in_dir(std::path::Path::new(&vp), &filename, 1);
                            }
                            match found {
                                Some(p) => p,
                                None => return Err(format!("找不到筆記：{}", rel)),
                            }
                        };

                        // 計算 rel 路徑（for DB）
                        let vault_base = std::path::PathBuf::from(&vp);
                        let abs_pb = std::path::PathBuf::from(&abs_path);
                        let rel_for_db = abs_pb.strip_prefix(&vault_base)
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| rel.clone());

                        std::fs::remove_file(&abs_path)
                            .map_err(|e| format!("刪除檔案失敗：{}", e))?;

                        let _ = db.query(
                            "DELETE FROM notes WHERE vault_id = $vid AND path = $path"
                        )
                        .bind(("vid", vid.clone()))
                        .bind(("path", rel_for_db.clone()))
                        .await;

                        Ok(Value::String(format!("已刪除筆記：{}", rel_for_db)))
                    })
                }),
                rollback: None,
            },
        );
    }

    // delete_folder — 寫入工具（需確認）
    {
        let vp = vault_path.clone();
        let db = vault_db.clone();
        let vid = vault_id.clone();
        registry.register(
            "delete_folder".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let vp = vp.clone();
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        if path.is_empty() {
                            return Err("請提供資料夾路徑".to_string());
                        }
                        let abs = std::path::PathBuf::from(&vp).join(&path);
                        if !abs.exists() {
                            return Err(format!("找不到資料夾：{}", path));
                        }
                        std::fs::remove_dir_all(&abs)
                            .map_err(|e| format!("刪除資料夾失敗：{}", e))?;

                        let prefix = if path.ends_with('/') {
                            path.clone()
                        } else {
                            format!("{}/", path)
                        };

                        let _ = db.query(
                            "DELETE FROM notes WHERE vault_id = $vid AND string::starts_with(path, $prefix)"
                        )
                        .bind(("vid", vid.clone()))
                        .bind(("prefix", prefix))
                        .await;

                        Ok(Value::String(format!("已刪除資料夾：{}", path)))
                    })
                }),
                rollback: None,
            },
        );
    }

    // move_note — 寫入工具
    {
        let vp = vault_path.clone();
        let db = vault_db.clone();
        let vid = vault_id.clone();
        registry.register(
            "move_note".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let from = args["from"].as_str().unwrap_or("").to_string();
                    let to = args["to"].as_str().unwrap_or("").to_string();
                    let vp = vp.clone();
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        if from.is_empty() || to.is_empty() {
                            return Err("from 和 to 為必填".to_string());
                        }
                        let from_rel = if from.ends_with(".md") { from.clone() } else { format!("{}.md", from) };
                        let to_rel   = if to.ends_with(".md")   { to.clone()   } else { format!("{}.md", to)   };

                        let from_abs = std::path::PathBuf::from(&vp).join(&from_rel);
                        let to_abs   = std::path::PathBuf::from(&vp).join(&to_rel);

                        if !from_abs.exists() {
                            return Err(format!("找不到來源筆記：{}", from_rel));
                        }
                        if let Some(parent) = to_abs.parent() {
                            std::fs::create_dir_all(parent)
                                .map_err(|e| format!("建立目標資料夾失敗：{}", e))?;
                        }
                        std::fs::rename(&from_abs, &to_abs)
                            .map_err(|e| format!("移動失敗：{}", e))?;

                        let _ = db.query(
                            "UPDATE notes SET path = $to WHERE vault_id = $vid AND path = $from"
                        )
                        .bind(("vid", vid.clone()))
                        .bind(("from", from_rel.clone()))
                        .bind(("to", to_rel.clone()))
                        .await;

                        Ok(Value::String(format!("已移動 {} → {}", from_rel, to_rel)))
                    })
                }),
                rollback: None,
            },
        );
    }

    // show_toast — 側效工具，無 rollback
    {
        let app_st = app.clone();
        registry.register(
            "show_toast".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let message = args["message"].as_str().unwrap_or("").to_string();
                    let kind = args["kind"].as_str().unwrap_or("info").to_string();
                    let duration_ms = args["duration_ms"].as_i64().unwrap_or(3000);
                    let app = app_st.clone();
                    Box::pin(async move {
                        let _ = app.emit("ui:toast", serde_json::json!({
                            "message": message,
                            "kind": kind,
                            "duration_ms": duration_ms,
                        }));
                        Ok(Value::String("已顯示通知".to_string()))
                    })
                }),
                rollback: None,
            },
        );
    }

    // ui_action — 側效工具，無 rollback
    {
        let app_ua = app.clone();
        registry.register(
            "ui_action".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let action = args["action"].as_str().unwrap_or("").to_string();
                    let payload = args["payload"].clone();
                    let app = app_ua.clone();
                    Box::pin(async move {
                        if action.is_empty() {
                            return Err("action 為必填".to_string());
                        }
                        let _ = app.emit("ui:action", serde_json::json!({
                            "action": action,
                            "payload": payload,
                        }));
                        Ok(Value::String(format!("已執行 UI 操作: {}", action)))
                    })
                }),
                rollback: None,
            },
        );
    }

    // reflect_on_skills — 唯讀，查詢技能規範統計
    {
        let db = vault_db.clone();
        let vid = vault_id.clone();
        registry.register(
            "reflect_on_skills".into(),
            Tool {
                execute: Arc::new(move |_args: Value| {
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        #[derive(serde::Deserialize)]
                        #[allow(dead_code)]
                        struct SkillRow {
                            skill_id: String,
                            title: String,
                            trigger: String,
                            trigger_count: Option<i64>,
                            last_triggered_at: Option<surrealdb::sql::Datetime>,
                            is_active: bool,
                            injection_mode: Option<String>,
                        }

                        let mut resp = db.query(
                            "SELECT skill_id, title, trigger, trigger_count, last_triggered_at, is_active, injection_mode \
                             FROM agent_skills \
                             WHERE vault_id = $vid AND is_active = true \
                             ORDER BY trigger_count DESC"
                        )
                        .bind(("vid", vid.clone()))
                        .await
                        .map_err(|e| e.to_string())?;

                        let rows: Vec<SkillRow> = resp.take(0).unwrap_or_default();

                        if rows.is_empty() {
                            return Ok(Value::String("目前沒有已啟用的技能規範。".to_string()));
                        }

                        let lines: Vec<String> = rows.iter().map(|s| {
                            let last = s.last_triggered_at.as_ref()
                                .map(|dt| dt.to_string())
                                .unwrap_or_else(|| "從未".to_string());
                            let count = s.trigger_count.unwrap_or(0);
                            let mode = s.injection_mode.as_deref().unwrap_or("passive");
                            format!(
                                "- **{}** (id: `{}`) | 觸發: {} | 最後: {} | 模式: {} | trigger: {}",
                                s.title, s.skill_id, count, last, mode, s.trigger
                            )
                        }).collect();

                        Ok(Value::String(format!(
                            "# 已啟用技能規範（共 {}）\n{}",
                            rows.len(), lines.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // search_web — 唯讀，搜尋網路（Brave Search）
    {
        let db = vault_db.clone();
        let vid = vault_id.clone();
        let app_sw = app.clone();
        let emb_sw = emb_url.clone();
        registry.register(
            "search_web".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let query = args["query"].as_str().unwrap_or("").to_string();
                    let db = db.clone();
                    let vid = vid.clone();
                    let app = app_sw.clone();
                    let emb = emb_sw.clone();
                    Box::pin(async move {
                        Ok(Value::String(
                            tool_web_search(&db, &vid, &query, &app, emb.as_deref()).await,
                        ))
                    })
                }),
                rollback: None,
            },
        );
    }

    // schedule_task — 寫入工具，插入 scheduled_tasks 表
    {
        let db = vault_db.clone();
        let vid = vault_id.clone();
        registry.register(
            "schedule_task".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let description = args["description"].as_str().unwrap_or("").to_string();
                    let run_at_str = args["run_at"].as_str().unwrap_or("").to_string();
                    let repeat_interval_secs = args["repeat_interval_seconds"].as_i64().unwrap_or(0);
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        if description.is_empty() {
                            return Err("description 為必填".to_string());
                        }
                        if run_at_str.is_empty() {
                            return Err("run_at 為必填".to_string());
                        }
                        let run_at_dt = chrono::DateTime::parse_from_rfc3339(&run_at_str)
                            .map_err(|e| format!("run_at 格式錯誤（需 ISO 8601）：{}", e))?;
                        let run_at_ts = run_at_dt.timestamp();
                        let task_id = uuid::Uuid::new_v4().to_string();
                        let now_ts = chrono::Utc::now().timestamp();

                        let _ = db.query(
                            "INSERT INTO scheduled_tasks (task_id, vault_id, description, run_at_ts, repeat_interval_secs, status, created_at) \
                             VALUES ($tid, $vid, $desc, $ts, $interval, 'pending', $now)"
                        )
                        .bind(("tid", task_id.clone()))
                        .bind(("vid", vid.clone()))
                        .bind(("desc", description.clone()))
                        .bind(("ts", run_at_ts))
                        .bind(("interval", repeat_interval_secs))
                        .bind(("now", now_ts))
                        .await
                        .map_err(|e| format!("排程失敗：{}", e))?;

                        let repeat_info = if repeat_interval_secs > 0 {
                            format!("，每 {} 秒重複", repeat_interval_secs)
                        } else {
                            String::new()
                        };

                        Ok(Value::String(format!(
                            "已排程「{}」於 {}{}（task_id: {}）",
                            description, run_at_str, repeat_info, task_id
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    Arc::new(registry)
}

/// 建立延遲繫結 LlmFn 的 Arc（供 build_vault_registry + invoke_agent 共用）
pub fn make_late_llm_fn() -> Arc<Mutex<Option<LlmFn>>> {
    Arc::new(Mutex::new(None))
}
