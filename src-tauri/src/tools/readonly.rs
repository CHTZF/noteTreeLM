/// tools/readonly.rs — 唯讀工具註冊
use std::sync::Arc;

use serde_json::Value;

use crate::commands::ai::{tool_list_structure, tool_read_note, tool_search_vault};
use crate::commands::knowledge_import::tool_web_search;
use crate::runtime::memory_agent::tool_query_memory;
use crate::runtime::tool_registry::ToolRegistry;
use crate::runtime::types::Tool;

use super::BuildCtx;

pub fn register(registry: &mut ToolRegistry, ctx: &BuildCtx) {
    // list_structure
    {
        let vp = ctx.vault_path.clone();
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
        let vp = ctx.vault_path.clone();
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
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        let app = ctx.app.clone();
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
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        let emb = ctx.emb_url.clone();
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

    // get_current_datetime — 回傳本地時間字串
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

    // list_notes_in_folder
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
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

    // reflect_on_skills
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
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

    // search_web — 搜尋網路（Brave Search）
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        let app = ctx.app.clone();
        let emb = ctx.emb_url.clone();
        registry.register(
            "search_web".into(),
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

}
