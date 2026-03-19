/// tools/mod.rs
/// 將現有 vault 工具函數包裝成 runtime ToolRegistry 格式。
///
/// 每個工具對應一個 `Tool { execute, rollback }`：
/// - 唯讀工具（list_structure / read_note / search_vault / query_memory）：rollback = None
/// - 寫入工具（create_note / update_note / create_folder）：rollback 實作還原邏輯
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

use crate::commands::ai::{
    resolve_vault_path, tool_create_folder, tool_create_note, tool_list_structure,
    tool_read_note, tool_search_vault, tool_update_note, call_external_ai_via_db,
    tool_list_recent_conversations, tool_create_agent_skill, vault_tools,
};
use crate::commands::knowledge_import::tool_web_search;
use crate::db::surreal::SurrealDb;
use crate::runtime::memory_agent::{add_memory_rule_to_db, tool_query_memory};
use crate::runtime::sub_agent::run_sub_agent;
use crate::runtime::tool_registry::ToolRegistry;
use crate::runtime::types::{LlmFn, Tool};

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
    {
        let app = app.clone();
        registry.register(
            "open_note".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let app = app.clone();
                    let mut path = args["path"].as_str().unwrap_or("").to_string();
                    if !path.is_empty() && !path.ends_with(".md") {
                        path.push_str(".md");
                    }
                    Box::pin(async move {
                        if path.is_empty() {
                            return Err("請提供筆記路徑".to_string());
                        }
                        let _ = app.emit("ui:open_note", &path);
                        Ok(Value::String(format!("✅ 已打開筆記：{}", path)))
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
                            call_external_ai_via_db(&query, &db, &app).await
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

    // spawn_sub_agent — 委派子任務給專門的 Sub-Agent 執行
    // 使用延遲繫結 LlmFn + Registry（invoke_agent 在 Arc::new(registry) 後設定）
    {
        let app_sa   = app.clone();
        let llm_late = Arc::clone(&llm_fn_late);
        let reg_late = Arc::clone(&registry_late);

        registry.register(
            "spawn_sub_agent".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let kind    = args["kind"].as_str().unwrap_or("custom").to_string();
                    let task    = args["task"].as_str().unwrap_or("").to_string();
                    let context = args["context"].as_str().unwrap_or("").to_string();
                    let llm_late = Arc::clone(&llm_late);
                    let reg_late = Arc::clone(&reg_late);
                    let app = app_sa.clone();
                    Box::pin(async move {
                        let llm_fn = {
                            let guard = llm_late.lock().await;
                            match guard.clone() {
                                Some(f) => f,
                                None => return Err("spawn_sub_agent: llm_fn 尚未初始化".to_string()),
                            }
                        };
                        let sub_registry = {
                            let guard = reg_late.lock().await;
                            match guard.clone() {
                                Some(r) => r,
                                None => return Err("spawn_sub_agent: registry 尚未初始化".to_string()),
                            }
                        };
                        let sub_session_id = uuid::Uuid::new_v4().to_string();
                        let tools_json = vault_tools();
                        let emit: crate::runtime::types::EmitEventFn = {
                            let app = app.clone();
                            Arc::new(move |event: String, payload: Value| {
                                let _ = tauri::Emitter::emit(&app, &event, payload);
                            })
                        };
                        let result = run_sub_agent(
                            sub_session_id,
                            String::new(),
                            &kind,
                            &task,
                            &context,
                            tools_json,
                            sub_registry,
                            llm_fn,
                            emit,
                        ).await;
                        Ok(Value::String(result))
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
