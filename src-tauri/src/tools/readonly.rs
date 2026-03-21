/// tools/readonly.rs — 唯讀工具註冊
use std::sync::Arc;

use serde_json::Value;
use tauri::Emitter;

use crate::commands::ai::{tool_list_structure, tool_read_note, tool_search_vault, search_skills_for_tool, get_embedding};
use crate::commands::knowledge_import::tool_web_search;
use crate::runtime::memory_agent::tool_query_memory;
use crate::runtime::tool_registry::ToolRegistry;
use crate::runtime::types::Tool;

use super::BuildCtx;

fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() { return 0.0; }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { return 0.0; }
    dot / (na * nb)
}

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

    // find_similar_notes — 向量語意搜尋相似筆記
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        let emb = ctx.emb_url.clone();
        registry.register(
            "find_similar_notes".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let query = args["query"].as_str().unwrap_or("").to_string();
                    let limit = args["limit"].as_u64().unwrap_or(5) as usize;
                    let db = db.clone();
                    let vid = vid.clone();
                    let emb = emb.clone();
                    Box::pin(async move {
                        if query.is_empty() {
                            return Err("請提供搜尋查詢".to_string());
                        }
                        let url = match emb {
                            Some(u) => u,
                            None => return Ok(Value::String("嵌入服務未啟動，無法進行語意搜尋".to_string())),
                        };
                        let client = reqwest::Client::new();
                        let q_emb = get_embedding(&client, &url, &query).await;
                        if q_emb.is_empty() {
                            return Ok(Value::String("無法取得查詢嵌入向量".to_string()));
                        }
                        #[derive(serde::Deserialize)]
                        struct ChunkRow { file_path: String, embedding: Option<Vec<f32>> }
                        let mut resp = db.query(
                            "SELECT file_path, embedding FROM chunks WHERE vault_id = $vid AND embedding != NONE LIMIT 2000"
                        )
                        .bind(("vid", vid.clone()))
                        .await
                        .map_err(|e| e.to_string())?;
                        let rows: Vec<ChunkRow> = resp.take(0).unwrap_or_default();
                        let mut file_scores: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
                        for row in &rows {
                            if let Some(ev) = &row.embedding {
                                let sim = cosine_sim(&q_emb, ev);
                                let entry = file_scores.entry(row.file_path.clone()).or_insert(0.0f32);
                                if sim > *entry { *entry = sim; }
                            }
                        }
                        let mut sorted: Vec<(String, f32)> = file_scores.into_iter().collect();
                        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                        sorted.truncate(limit.min(20));
                        if sorted.is_empty() {
                            return Ok(Value::String(format!("找不到與「{}」相似的筆記", query)));
                        }
                        let lines: Vec<String> = sorted.iter().map(|(p, s)| {
                            format!("- {} (相似度: {:.2})", p, s)
                        }).collect();
                        Ok(Value::String(format!(
                            "與「{}」相似的筆記（前 {}）：\n{}",
                            query, sorted.len(), lines.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // get_note_backlinks — 反向連結查詢（哪些筆記連結至指定筆記）
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "get_note_backlinks".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        if path.is_empty() {
                            return Err("請提供筆記路徑".to_string());
                        }
                        let name_no_ext = std::path::Path::new(&path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.trim_end_matches(".md").to_string());
                        let link_pattern = format!("[[{}]]", name_no_ext);
                        let path_ref = format!("({})", path);
                        #[derive(serde::Deserialize)]
                        struct NoteRow { path: String, title: Option<String> }
                        let mut resp = db.query(
                            "SELECT path, title FROM notes WHERE vault_id = $vid \
                             AND (string::contains(content, $link) OR string::contains(content, $pref)) \
                             AND path != $self ORDER BY modified_at DESC LIMIT 50"
                        )
                        .bind(("vid", vid.clone()))
                        .bind(("link", link_pattern))
                        .bind(("pref", path_ref))
                        .bind(("self", path.clone()))
                        .await
                        .map_err(|e| e.to_string())?;
                        let rows: Vec<NoteRow> = resp.take(0).unwrap_or_default();
                        if rows.is_empty() {
                            return Ok(Value::String(format!("找不到連結至「{}」的筆記", path)));
                        }
                        let lines: Vec<String> = rows.iter().map(|r| {
                            let title = r.title.as_deref().unwrap_or("(無標題)");
                            format!("- {} ({})", title, r.path)
                        }).collect();
                        Ok(Value::String(format!(
                            "連結至「{}」的筆記（共 {}）：\n{}",
                            path, lines.len(), lines.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // get_vault_stats — 知識庫統計（筆記數、資料夾數、字數、最近修改）
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "get_vault_stats".into(),
            Tool {
                execute: Arc::new(move |_args: Value| {
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        #[derive(serde::Deserialize)]
                        struct NoteRow {
                            path: String,
                            word_count: Option<i64>,
                            modified_at: Option<surrealdb::sql::Datetime>,
                        }
                        let mut resp = db.query(
                            "SELECT path, word_count, modified_at FROM notes WHERE vault_id = $vid ORDER BY modified_at DESC"
                        )
                        .bind(("vid", vid.clone()))
                        .await
                        .map_err(|e| e.to_string())?;
                        let rows: Vec<NoteRow> = resp.take(0).unwrap_or_default();
                        let total_notes = rows.len();
                        let total_words: i64 = rows.iter().filter_map(|r| r.word_count).sum();
                        let mut folders: std::collections::HashSet<String> = std::collections::HashSet::new();
                        for row in &rows {
                            if let Some(parent) = std::path::Path::new(&row.path).parent() {
                                let f = parent.to_string_lossy().to_string();
                                if !f.is_empty() { folders.insert(f); }
                            }
                        }
                        let recent: Vec<String> = rows.iter().take(5).map(|r| {
                            let ts = r.modified_at.as_ref().map(|dt| dt.to_string()).unwrap_or_else(|| "未知".to_string());
                            format!("  - {} ({})", r.path, ts)
                        }).collect();
                        Ok(Value::String(format!(
                            "# 知識庫統計\n- 筆記總數：{} 篇\n- 資料夾數：{}\n- 總字數：{} 字\n- 最近修改：\n{}",
                            total_notes, folders.len(), total_words, recent.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // summarize_note_collection — 批次讀取多篇筆記並 LLM 摘要
    {
        let vp = ctx.vault_path.clone();
        let emb = ctx.emb_url.clone();
        registry.register(
            "summarize_note_collection".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let paths: Vec<String> = args["paths"].as_array()
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    let query = args["query"].as_str().unwrap_or("").to_string();
                    let vp = vp.clone();
                    let emb = emb.clone();
                    Box::pin(async move {
                        if paths.is_empty() {
                            return Err("請提供至少一個筆記路徑（paths 陣列）".to_string());
                        }
                        let mut combined = String::new();
                        let mut found_count = 0usize;
                        for path in &paths {
                            let rel = if path.ends_with(".md") { path.clone() } else { format!("{}.md", path) };
                            let abs = std::path::PathBuf::from(&vp).join(&rel);
                            if let Ok(content) = tokio::fs::read_to_string(&abs).await {
                                combined.push_str(&format!("\n\n## 筆記：{}\n{}", rel, &content.chars().take(3000).collect::<String>()));
                                found_count += 1;
                            }
                        }
                        if combined.is_empty() {
                            return Ok(Value::String("找不到指定的筆記".to_string()));
                        }
                        let base_url = match emb {
                            Some(u) => u,
                            None => return Ok(Value::String(format!("已讀取 {} 篇筆記（LLM 未啟動）：{}", found_count, combined))),
                        };
                        let client = reqwest::Client::new();
                        let system = "你是筆記整合助手。根據以下多篇筆記，輸出結構化摘要：條列主要概念、重要事項、各篇筆記的關聯性。用繁體中文，不超過 600 字。";
                        let user_msg = if query.is_empty() {
                            format!("請整合以下筆記：{}", combined)
                        } else {
                            format!("查詢重點：{}\n\n筆記內容：{}", query, combined)
                        };
                        let body = serde_json::json!({
                            "messages": [
                                {"role": "system", "content": system},
                                {"role": "user", "content": user_msg},
                            ],
                            "max_tokens": 800,
                            "temperature": 0.4,
                            "stream": false,
                        });
                        let resp = match client
                            .post(format!("{}/v1/chat/completions", base_url))
                            .json(&body)
                            .timeout(std::time::Duration::from_secs(40))
                            .send()
                            .await
                        {
                            Ok(r) if r.status().is_success() => r,
                            _ => return Ok(Value::String(format!("已讀取 {} 篇筆記（摘要失敗）：{}", found_count, combined))),
                        };
                        let json: serde_json::Value = match resp.json().await {
                            Ok(v) => v,
                            Err(_) => return Ok(Value::String(combined)),
                        };
                        let summary = json["choices"][0]["message"]["content"]
                            .as_str().unwrap_or("摘要失敗").to_string();
                        Ok(Value::String(format!("# 筆記集合摘要（共 {} 篇）\n\n{}", found_count, summary)))
                    })
                }),
                rollback: None,
            },
        );
    }

    // distill_preferences — 從對話記憶蒸餾使用者偏好
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        let emb = ctx.emb_url.clone();
        registry.register(
            "distill_preferences".into(),
            Tool {
                execute: Arc::new(move |_args: Value| {
                    let db = db.clone();
                    let vid = vid.clone();
                    let emb = emb.clone();
                    Box::pin(async move {
                        let base_url = match emb {
                            Some(u) => u,
                            None => return Ok(Value::String("LLM 未啟動，無法分析偏好".to_string())),
                        };
                        #[derive(serde::Deserialize)]
                        struct MemRow { content: String }
                        let mut resp = match db.query(
                            "SELECT content FROM notes \
                             WHERE vault_id = $vid AND string::starts_with(path, 'memories/') \
                             ORDER BY modified_at DESC LIMIT 5"
                        )
                        .bind(("vid", vid.clone()))
                        .await {
                            Ok(r) => r,
                            Err(_) => return Ok(Value::String("無法讀取記憶".to_string())),
                        };
                        let rows: Vec<MemRow> = resp.take(0).unwrap_or_default();
                        if rows.is_empty() {
                            return Ok(Value::String("目前沒有對話記憶可供分析".to_string()));
                        }
                        let combined: String = rows.iter()
                            .map(|r| r.content.chars().take(2000).collect::<String>())
                            .collect::<Vec<_>>()
                            .join("\n\n---\n\n");
                        let client = reqwest::Client::new();
                        let body = serde_json::json!({
                            "messages": [
                                {"role": "system", "content": "你是使用者偏好分析系統。從對話記憶中提取：1) 工作習慣與偏好 2) 常見需求模式 3) 個人背景 4) 明確規則。輸出條列式，每條以「- 」開頭，不超過 15 條，只輸出列表。"},
                                {"role": "user", "content": format!("以下是最近對話記憶：\n\n{}", combined)},
                            ],
                            "max_tokens": 512,
                            "temperature": 0.3,
                            "stream": false,
                        });
                        let http_resp = match client
                            .post(format!("{}/v1/chat/completions", base_url))
                            .json(&body)
                            .timeout(std::time::Duration::from_secs(30))
                            .send()
                            .await
                        {
                            Ok(r) if r.status().is_success() => r,
                            _ => return Ok(Value::String("偏好分析失敗（LLM 無回應）".to_string())),
                        };
                        let json: serde_json::Value = match http_resp.json().await {
                            Ok(v) => v,
                            Err(_) => return Ok(Value::String("偏好分析回應解析失敗".to_string())),
                        };
                        let prefs = json["choices"][0]["message"]["content"]
                            .as_str().unwrap_or("無法解析偏好").to_string();
                        Ok(Value::String(format!("# 使用者偏好分析\n\n{}", prefs)))
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
