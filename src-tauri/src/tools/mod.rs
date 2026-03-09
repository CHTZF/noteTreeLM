/// tools/mod.rs
/// 將現有 vault 工具函數包裝成 runtime ToolRegistry 格式。
///
/// 每個工具對應一個 `Tool { execute, rollback }`：
/// - 唯讀工具（list_structure / read_note / search_vault / query_memory）：rollback = None
/// - 寫入工具（create_note / update_note / create_folder）：rollback 實作還原邏輯
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use sqlx::SqlitePool;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

use crate::commands::ai::{
    resolve_vault_path, tool_create_folder, tool_create_note, tool_list_structure,
    tool_read_note, tool_search_vault, tool_update_note,
};
use crate::runtime::memory_agent::{add_memory_rule_to_db, tool_query_memory};
use crate::runtime::tool_registry::ToolRegistry;
use crate::runtime::types::Tool;

/// 建立包含所有 vault 工具的 ToolRegistry。
///
/// # 參數
/// - `vault_path`: Vault 根目錄絕對路徑
/// - `vault_db`:   Vault SQLite 連線池（search / memory 工具使用）
/// - `app`:        Tauri AppHandle（search 工具 emit debug 事件使用）
pub fn build_vault_registry(
    vault_path: String,
    vault_db: SqlitePool,
    app: AppHandle,
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
        let vp = vault_path.clone();
        let db = vault_db.clone();
        let app = app.clone();
        registry.register(
            "search_vault".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let query = args["query"].as_str().unwrap_or("").to_string();
                    let db = db.clone();
                    let app = app.clone();
                    Box::pin(async move {
                        Ok(Value::String(tool_search_vault(&query, &db, &app).await))
                    })
                }),
                rollback: None,
            },
        );
    }

    // query_memory
    {
        let db = vault_db.clone();
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
                    Box::pin(async move {
                        Ok(Value::String(tool_query_memory(keywords, since, limit, &db).await))
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
        registry.register(
            "create_note".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let content = args["content"].as_str().unwrap_or("").to_string();
                    let vp = vp_exec.clone();
                    Box::pin(async move {
                        let result = tool_create_note(&path, &content, &vp).await;
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
                    let backups = Arc::clone(&backups);
                    Box::pin(async move {
                        let abs_path = resolve_vault_path(&path, &vp).map_err(|e| e)?;
                        // 先備份原始內容
                        let original =
                            tokio::fs::read_to_string(&abs_path).await.unwrap_or_default();
                        backups.lock().await.insert(path.clone(), original);
                        // 寫入新內容
                        tokio::fs::write(&abs_path, &content)
                            .await
                            .map_err(|e| format!("更新失敗：{}", e))?;
                        Ok(Value::String(format!("✅ 已更新筆記：{}", path)))
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
        registry.register(
            "add_memory_rule".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let ptype   = args["pattern_type"].as_str().unwrap_or("").to_string();
                    let pattern = args["pattern"].as_str().unwrap_or("").to_string();
                    let value   = args["value"].as_str().unwrap_or("").to_string();
                    let db = db.clone();
                    Box::pin(async move {
                        Ok(Value::String(add_memory_rule_to_db(&db, &ptype, &pattern, &value).await))
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

    // TODO: call_external_ai — 需要 ExtAiConfig，待後續整合

    Arc::new(registry)
}
