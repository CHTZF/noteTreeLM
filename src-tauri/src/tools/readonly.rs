/// tools/readonly.rs — 唯讀工具註冊
use std::sync::Arc;

use serde_json::Value;
use tauri::Emitter;

use crate::commands::ai::{tool_list_structure, tool_read_note, tool_search_vault, search_skills_for_tool};
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
        let sqlite = ctx.sqlite.clone();
        let app = ctx.app.clone();
        registry.register(
            "search_vault".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let query = args["query"].as_str().unwrap_or("").to_string();
                    let db = db.clone();
                    let vid = vid.clone();
                    let sqlite = sqlite.clone();
                    let app = app.clone();
                    Box::pin(async move {
                        Ok(Value::String(tool_search_vault(&query, &db, &vid, Some(&sqlite), &app).await))
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

    // search_skills — LLM 自主語意搜尋技能規範（use_ask = 標準化意圖概括）
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        let emb = ctx.emb_url.clone();
        let app = ctx.app.clone();
        registry.register(
            "search_skills".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let use_ask = args["use_ask"].as_str().unwrap_or("").to_string();
                    let db = db.clone();
                    let vid = vid.clone();
                    let emb = emb.clone();
                    let app = app.clone();
                    Box::pin(async move {
                        if use_ask.is_empty() {
                            return Ok(Value::String("請提供 use_ask 參數".to_string()));
                        }
                        let client = reqwest::Client::new();
                        let skills = search_skills_for_tool(
                            &db, &vid, &use_ask, emb.as_deref(), &client,
                        ).await;

                        if skills.is_empty() {
                            // 通知前端：找不到技能，讓使用者選擇要加入哪個 skill 的觸發條件
                            let _ = app.emit("agent:skill_not_found", serde_json::json!({
                                "use_ask": use_ask,
                            }));
                            return Ok(Value::String(serde_json::json!({
                                "behavior": "找不到相關技能規範，請直接回應使用者。",
                                "required_tools": []
                            }).to_string()));
                        }

                        // 通知前端：找到匹配技能，詢問是否將 use_ask 加入觸發條件（只問第一個）
                        let (first_id, first_title, _, _) = &skills[0];
                        let _ = app.emit("agent:skill_found", serde_json::json!({
                            "skill_id": first_id,
                            "skill_title": first_title,
                            "use_ask": use_ask,
                        }));

                        // 收集所有命中 skill 的 tool_calls 聯集
                        let required_tools: Vec<String> = skills.iter()
                            .flat_map(|(_, _, _, tools)| tools.iter().cloned())
                            .collect::<std::collections::HashSet<_>>()
                            .into_iter()
                            .collect();

                        // behavior 文字：LLM 主動搜尋的結果，需自行評估選擇最合適的
                        // 語氣與自動觸發不同：自動觸發是「遵守」，搜尋是「評估後選擇執行」
                        let mut behavior = format!(
                            "# 技能搜尋結果（意圖：「{}」）\n\
                             以下是可能符合的技能規範，請根據使用者實際需求選擇最合適的一個執行，\
                             若多個都適用則以第一個為主：\n\n",
                            use_ask
                        );
                        for (idx, (_, title, beh, tool_names)) in skills.iter().enumerate() {
                            behavior.push_str(&format!(
                                "## 候選 {}：{}\n**執行方式**：{}\n**可用工具**：{}\n\n",
                                idx + 1, title, beh,
                                if tool_names.is_empty() { "（通用）".to_string() } else { tool_names.join("、") },
                            ));
                        }

                        // 回傳 JSON：behavior 給 LLM 看，required_tools 供 tool loop 動態注入
                        Ok(Value::String(serde_json::json!({
                            "behavior": behavior,
                            "required_tools": required_tools,
                        }).to_string()))
                    })
                }),
                rollback: None,
            },
        );
    }

}
