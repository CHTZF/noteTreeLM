/// tools/readonly.rs — 唯讀工具註冊（daemon API 版）
use std::sync::Arc;

use serde_json::Value;
use tauri::Emitter;

use crate::commands::ai::{tool_list_structure, tool_read_note, search_skills_for_tool, get_embedding};
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

    // search_vault — daemon search endpoint
    {
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        let app = ctx.app.clone();
        registry.register(
            "search_vault".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let query = args["query"].as_str().unwrap_or("").to_string();
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    let app = app.clone();
                    Box::pin(async move {
                        let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &client,
                            &format!("/vaults/{}/search?q={}", urlencoding::encode(&vid), urlencoding::encode(&query)),
                            tok_ref,
                        ).await.unwrap_or_else(|_| serde_json::json!([]));
                        let rows = result.as_array().cloned().unwrap_or_default();
                        if rows.is_empty() {
                            return Ok(Value::String(format!("未找到與「{}」相關的筆記", query)));
                        }
                        let lines: Vec<String> = rows.iter().take(5).map(|r| {
                            let path = r["path"].as_str().unwrap_or("");
                            let section = r["section"].as_str().unwrap_or("").chars().take(200).collect::<String>();
                            format!("**{}**\n{}", path, section)
                        }).collect();
                        let _ = app.emit("agent:note_refs", serde_json::json!(rows.iter().map(|r| r["path"].as_str().unwrap_or("")).collect::<Vec<_>>()));
                        Ok(Value::String(lines.join("\n\n---\n\n")))
                    })
                }),
                rollback: None,
            },
        );
    }

    // query_memory — daemon search endpoint filtered to memories/
    {
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "query_memory".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let keywords: Vec<String> = args["keywords"]
                        .as_array()
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    let limit = args["limit"].as_u64().unwrap_or(3) as usize;
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                        let kw_param = urlencoding::encode(&keywords.join(",")).to_string();
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &client,
                            &format!("/vaults/{}/memory/query?keywords={}&limit={}", urlencoding::encode(&vid), kw_param, limit),
                            tok_ref,
                        ).await.unwrap_or_else(|_| serde_json::json!([]));
                        let rows = result.as_array().cloned().unwrap_or_default();
                        if rows.is_empty() {
                            return Ok(Value::String("未找到相關記憶".to_string()));
                        }
                        let lines: Vec<String> = rows.iter().map(|r| {
                            let path = r["path"].as_str().unwrap_or("");
                            let snippet = r["snippet"].as_str().unwrap_or("").chars().take(400).collect::<String>();
                            format!("【{}】\n{}", path, snippet)
                        }).collect();
                        Ok(Value::String(lines.join("\n\n---\n\n")))
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

    // list_notes_in_folder — daemon search with path_prefix
    {
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "list_notes_in_folder".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let folder = args["folder"].as_str().unwrap_or("").to_string();
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        if folder.is_empty() {
                            return Err("請提供資料夾路徑".to_string());
                        }
                        let prefix = if folder.ends_with('/') { folder.clone() } else { format!("{}/", folder) };
                        let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &client,
                            &format!("/vaults/{}/notes?path_prefix={}", urlencoding::encode(&vid), urlencoding::encode(&prefix)),
                            tok_ref,
                        ).await.unwrap_or_else(|_| serde_json::json!([]));
                        let notes = result.as_array().cloned().unwrap_or_default();
                        if notes.is_empty() {
                            return Ok(Value::String(format!("資料夾「{}」中沒有筆記。", folder)));
                        }
                        let lines: Vec<String> = notes.iter().map(|r| {
                            let title = r["title"].as_str().unwrap_or("(無標題)");
                            let path = r["path"].as_str().unwrap_or("");
                            format!("- {} ({})", title, path)
                        }).collect();
                        Ok(Value::String(format!(
                            "資料夾「{}」共 {} 篇筆記：\n{}", folder, lines.len(), lines.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // reflect_on_skills — daemon GET /vaults/:vid/skills
    {
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "reflect_on_skills".into(),
            Tool {
                execute: Arc::new(move |_args: Value| {
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &client,
                            &format!("/vaults/{}/skills", urlencoding::encode(&vid)),
                            tok_ref,
                        ).await.unwrap_or_else(|_| serde_json::json!([]));
                        let skills = result.as_array().cloned().unwrap_or_default();
                        if skills.is_empty() {
                            return Ok(Value::String("目前沒有已啟用的技能規範。".to_string()));
                        }
                        let lines: Vec<String> = skills.iter().filter(|s| {
                            s["is_active"].as_bool().unwrap_or(true)
                        }).map(|s| {
                            let title = s["title"].as_str().unwrap_or("(無標題)");
                            let skill_id = s["skill_id"].as_str().unwrap_or("");
                            let trigger = s["trigger"].as_str().unwrap_or("");
                            let count = s["trigger_count"].as_i64().unwrap_or(0);
                            let mode = s["injection_mode"].as_str().unwrap_or("passive");
                            format!(
                                "- **{}** (id: `{}`) | 觸發: {} | 模式: {} | trigger: {}",
                                title, skill_id, count, mode, trigger
                            )
                        }).collect();
                        Ok(Value::String(format!(
                            "# 已啟用技能規範（共 {}）\n{}", lines.len(), lines.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // find_similar_notes — daemon search (semantic similarity via daemon)
    {
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        let emb = ctx.emb_url.clone();
        registry.register(
            "find_similar_notes".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let query = args["query"].as_str().unwrap_or("").to_string();
                    let limit = args["limit"].as_u64().unwrap_or(5) as usize;
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    let emb = emb.clone();
                    Box::pin(async move {
                        if query.is_empty() {
                            return Err("請提供搜尋查詢".to_string());
                        }
                        let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                        // Semantic search via daemon (handles embedding internally if available)
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &client,
                            &format!("/vaults/{}/search?q={}", urlencoding::encode(&vid), urlencoding::encode(&query)),
                            tok_ref,
                        ).await.unwrap_or_else(|_| serde_json::json!([]));
                        let rows = result.as_array().cloned().unwrap_or_default();
                        let _ = emb; // suppress unused warning
                        if rows.is_empty() {
                            return Ok(Value::String(format!("找不到與「{}」相似的筆記", query)));
                        }
                        let lines: Vec<String> = rows.iter().map(|r| {
                            format!("- {}", r["path"].as_str().unwrap_or(""))
                        }).collect();
                        Ok(Value::String(format!(
                            "與「{}」相似的筆記（前 {}）：\n{}", query, lines.len(), lines.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // get_note_backlinks — search content for references to a note
    {
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "get_note_backlinks".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        if path.is_empty() {
                            return Err("請提供筆記路徑".to_string());
                        }
                        let name_no_ext = std::path::Path::new(&path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.trim_end_matches(".md").to_string());
                        let link_query = format!("[[{}]]", name_no_ext);
                        let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &client,
                            &format!("/vaults/{}/search?q={}", urlencoding::encode(&vid), urlencoding::encode(&link_query)),
                            tok_ref,
                        ).await.unwrap_or_else(|_| serde_json::json!([]));
                        let rows = result.as_array().cloned().unwrap_or_default();
                        if rows.is_empty() {
                            return Ok(Value::String(format!("找不到連結至「{}」的筆記", path)));
                        }
                        let lines: Vec<String> = rows.iter().map(|r| {
                            let p = r["path"].as_str().unwrap_or("");
                            format!("- {}", p)
                        }).collect();
                        Ok(Value::String(format!(
                            "連結至「{}」的筆記（共 {}）：\n{}", path, lines.len(), lines.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // get_vault_stats — daemon GET /vaults/:vid/stats
    {
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "get_vault_stats".into(),
            Tool {
                execute: Arc::new(move |_args: Value| {
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &client,
                            &format!("/vaults/{}/stats", urlencoding::encode(&vid)),
                            tok_ref,
                        ).await.unwrap_or_else(|_| serde_json::json!({}));
                        let total_notes = result["total"].as_i64().unwrap_or(0);
                        let chunked = result["chunked"].as_i64().unwrap_or(0);
                        let embedded = result["embedded"].as_i64().unwrap_or(0);
                        Ok(Value::String(format!(
                            "# 知識庫統計\n- 筆記總數：{} 篇\n- 已分塊：{} chunks\n- 已向量化：{}",
                            total_notes, chunked, embedded
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // summarize_note_collection — file-based, no DB needed
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

    // distill_preferences — daemon search memories, then LLM
    {
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        let emb = ctx.emb_url.clone();
        registry.register(
            "distill_preferences".into(),
            Tool {
                execute: Arc::new(move |_args: Value| {
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    let emb = emb.clone();
                    Box::pin(async move {
                        let base_url = match emb {
                            Some(u) => u,
                            None => return Ok(Value::String("LLM 未啟動，無法分析偏好".to_string())),
                        };
                        let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &client,
                            &format!("/vaults/{}/memory/query?keywords=&limit=5", urlencoding::encode(&vid)),
                            tok_ref,
                        ).await.unwrap_or_else(|_| serde_json::json!([]));
                        let rows = result.as_array().cloned().unwrap_or_default();
                        if rows.is_empty() {
                            return Ok(Value::String("目前沒有對話記憶可供分析".to_string()));
                        }
                        let combined: String = rows.iter()
                            .map(|r| r["snippet"].as_str().unwrap_or("").chars().take(2000).collect::<String>())
                            .collect::<Vec<_>>()
                            .join("\n\n---\n\n");
                        let http_client = reqwest::Client::new();
                        let body = serde_json::json!({
                            "messages": [
                                {"role": "system", "content": "你是使用者偏好分析系統。從對話記憶中提取：1) 工作習慣與偏好 2) 常見需求模式 3) 個人背景 4) 明確規則。輸出條列式，每條以「- 」開頭，不超過 15 條，只輸出列表。"},
                                {"role": "user", "content": format!("以下是最近對話記憶：\n\n{}", combined)},
                            ],
                            "max_tokens": 512,
                            "temperature": 0.3,
                            "stream": false,
                        });
                        let http_resp = match http_client
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

    // search_by_tag — daemon search
    {
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "search_by_tag".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let tag = args["tag"].as_str().unwrap_or("").to_string();
                    let limit = args["limit"].as_u64().unwrap_or(50) as usize;
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        if tag.is_empty() {
                            return Err("請提供 tag 名稱".to_string());
                        }
                        let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &client,
                            &format!("/vaults/{}/search?q={}", urlencoding::encode(&vid), urlencoding::encode(&tag)),
                            tok_ref,
                        ).await.unwrap_or_else(|_| serde_json::json!([]));
                        let rows = result.as_array().cloned().unwrap_or_default();
                        if rows.is_empty() {
                            return Ok(Value::String(format!("找不到標籤「{}」的筆記", tag)));
                        }
                        let lines: Vec<String> = rows.iter().map(|r| {
                            let path = r["path"].as_str().unwrap_or("");
                            format!("- {}", path)
                        }).collect();
                        Ok(Value::String(format!(
                            "標籤「{}」的筆記（共 {}）：\n{}", tag, lines.len(), lines.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // extract_action_items — file-based (uses vault_path)
    {
        let vp = ctx.vault_path.clone();
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "extract_action_items".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let folder = args["folder"].as_str().unwrap_or("").to_string();
                    let include_done = args["include_done"].as_bool().unwrap_or(false);
                    let vp = vp.clone();
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        let mut paths_to_scan: Vec<String> = Vec::new();
                        if !path.is_empty() {
                            let rel = if path.ends_with(".md") { path.clone() } else { format!("{}.md", path) };
                            paths_to_scan.push(rel);
                        } else if !folder.is_empty() {
                            let prefix = if folder.ends_with('/') { folder.clone() } else { format!("{}/", folder) };
                            let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                            let result: serde_json::Value = crate::api_client::daemon_get(
                                &client,
                                &format!("/vaults/{}/notes?path_prefix={}", urlencoding::encode(&vid), urlencoding::encode(&prefix)),
                                tok_ref,
                            ).await.unwrap_or_else(|_| serde_json::json!([]));
                            let notes = result.as_array().cloned().unwrap_or_default();
                            paths_to_scan = notes.iter().filter_map(|n| n["path"].as_str().map(String::from)).collect();
                        } else {
                            return Err("請提供 path（單一筆記）或 folder（資料夾）".to_string());
                        }

                        let mut all_items: Vec<String> = Vec::new();
                        for p in &paths_to_scan {
                            let abs = std::path::PathBuf::from(&vp).join(p);
                            let content = match tokio::fs::read_to_string(&abs).await {
                                Ok(c) => c,
                                Err(_) => continue,
                            };
                            let note_label = p.trim_end_matches(".md").split('/').last().unwrap_or(p);
                            for line in content.lines() {
                                let trimmed = line.trim();
                                let is_unchecked = trimmed.starts_with("- [ ]") || trimmed.starts_with("* [ ]");
                                let is_checked = trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]") || trimmed.starts_with("* [x]") || trimmed.starts_with("* [X]");
                                let is_todo = trimmed.to_uppercase().starts_with("TODO:") || trimmed.to_uppercase().starts_with("ACTION:") || trimmed.to_uppercase().starts_with("FIXME:");
                                if is_unchecked || is_todo || (include_done && is_checked) {
                                    let pfx = if is_checked { "✅" } else { "⬜" };
                                    all_items.push(format!("{} [{}] {}", pfx, note_label, trimmed));
                                }
                            }
                        }
                        if all_items.is_empty() {
                            return Ok(Value::String("找不到待辦事項".to_string()));
                        }
                        Ok(Value::String(format!(
                            "# 待辦事項（共 {}）\n{}", all_items.len(), all_items.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // find_orphan_notes — list all notes via daemon and compute orphans in memory
    {
        let vp = ctx.vault_path.clone();
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "find_orphan_notes".into(),
            Tool {
                execute: Arc::new(move |_args: Value| {
                    let vp = vp.clone();
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &client,
                            &format!("/vaults/{}/notes", urlencoding::encode(&vid)),
                            tok_ref,
                        ).await.unwrap_or_else(|_| serde_json::json!([]));
                        let notes = result.as_array().cloned().unwrap_or_default();
                        // Build referenced set from file content
                        let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
                        for note in &notes {
                            let path = note["path"].as_str().unwrap_or("");
                            let abs = std::path::PathBuf::from(&vp).join(path);
                            if let Ok(content) = tokio::fs::read_to_string(&abs).await {
                                let mut pos = 0;
                                while let Some(open) = content[pos..].find("[[") {
                                    let abs_open = pos + open + 2;
                                    if let Some(close) = content[abs_open..].find("]]") {
                                        let link = content[abs_open..abs_open + close].trim().to_lowercase();
                                        let link = link.split('|').next().unwrap_or(&link).trim().to_string();
                                        referenced.insert(link);
                                        pos = abs_open + close + 2;
                                    } else { break; }
                                }
                            }
                        }
                        let orphans: Vec<&serde_json::Value> = notes.iter().filter(|n| {
                            let path = n["path"].as_str().unwrap_or("");
                            let stem = std::path::Path::new(path)
                                .file_stem()
                                .map(|s| s.to_string_lossy().to_lowercase())
                                .unwrap_or_default();
                            if stem == "index" || stem == "moc" || stem == "readme" { return false; }
                            !referenced.contains(stem.as_str())
                        }).collect();
                        if orphans.is_empty() {
                            return Ok(Value::String("所有筆記都有反向連結，知識庫連結狀況良好！".to_string()));
                        }
                        let lines: Vec<String> = orphans.iter().map(|n| {
                            let title = n["title"].as_str().unwrap_or("(無標題)");
                            let path = n["path"].as_str().unwrap_or("");
                            format!("- {} ({})", title, path)
                        }).collect();
                        Ok(Value::String(format!(
                            "# 孤立筆記（無反向連結，共 {}）\n{}", orphans.len(), lines.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // list_recent_notes — daemon GET /vaults/:vid/notes with sort
    {
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "list_recent_notes".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let days = args["days"].as_u64().unwrap_or(7) as i64;
                    let limit = args["limit"].as_u64().unwrap_or(20) as usize;
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        let since_ts = chrono::Utc::now().timestamp() - days * 86400;
                        let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &client,
                            &format!("/vaults/{}/notes", urlencoding::encode(&vid)),
                            tok_ref,
                        ).await.unwrap_or_else(|_| serde_json::json!([]));
                        let notes = result.as_array().cloned().unwrap_or_default();
                        // Filter by days in Rust
                        let filtered: Vec<&serde_json::Value> = notes.iter().filter(|n| {
                            n["modified_at"].as_i64().map(|ts| ts >= since_ts).unwrap_or(false)
                        }).collect();
                        if filtered.is_empty() {
                            return Ok(Value::String(format!("最近 {} 天沒有修改過筆記", days)));
                        }
                        let lines: Vec<String> = filtered.iter().map(|n| {
                            let title = n["title"].as_str().unwrap_or("(無標題)");
                            let path = n["path"].as_str().unwrap_or("");
                            let wc = n["word_count"].as_i64().unwrap_or(0);
                            format!("- {} ({}) [{}字]", title, path, wc)
                        }).collect();
                        Ok(Value::String(format!(
                            "最近 {} 天修改的筆記（共 {}）：\n{}", days, filtered.len(), lines.join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // extract_note_links — file-based
    {
        let vp = ctx.vault_path.clone();
        registry.register(
            "extract_note_links".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let vp = vp.clone();
                    Box::pin(async move {
                        if path.is_empty() { return Err("請提供筆記路徑".to_string()); }
                        let rel = if path.ends_with(".md") { path.clone() } else { format!("{}.md", path) };
                        let abs = std::path::PathBuf::from(&vp).join(&rel);
                        let content = tokio::fs::read_to_string(&abs).await
                            .map_err(|e| format!("讀取失敗：{}", e))?;
                        let mut links: Vec<String> = Vec::new();
                        let mut pos = 0;
                        while let Some(open) = content[pos..].find("[[") {
                            let abs_open = pos + open + 2;
                            if let Some(close) = content[abs_open..].find("]]") {
                                let raw = &content[abs_open..abs_open + close];
                                let parts: Vec<&str> = raw.splitn(2, '|').collect();
                                let display = if parts.len() == 2 {
                                    format!("[[{}]] (別名: {})", parts[0].trim(), parts[1].trim())
                                } else {
                                    format!("[[{}]]", raw.trim())
                                };
                                if !links.contains(&display) { links.push(display); }
                                pos = abs_open + close + 2;
                            } else { break; }
                        }
                        if links.is_empty() {
                            return Ok(Value::String(format!("「{}」沒有出向連結", rel)));
                        }
                        Ok(Value::String(format!(
                            "「{}」的出向連結（共 {}）：\n{}", rel, links.len(),
                            links.iter().map(|l| format!("- {}", l)).collect::<Vec<_>>().join("\n")
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }

    // search_skills — uses search_skills_for_tool from ai.rs (needs migration too, but stub for now)
    {
        let client = ctx.http_client.clone();
        let tok = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        let emb = ctx.emb_url.clone();
        let app = ctx.app.clone();
        registry.register(
            "search_skills".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let use_ask = args["use_ask"].as_str().unwrap_or("").to_string();
                    let client = client.clone();
                    let tok = tok.clone();
                    let vid = vid.clone();
                    let emb = emb.clone();
                    let app = app.clone();
                    Box::pin(async move {
                        if use_ask.is_empty() {
                            return Ok(Value::String("請提供 use_ask 參數".to_string()));
                        }
                        let http_client = reqwest::Client::new();
                        let skills = search_skills_for_tool(
                            &client, &tok, &vid, &use_ask, emb.as_deref(), &http_client,
                        ).await;

                        if skills.is_empty() {
                            let _ = app.emit("agent:skill_not_found", serde_json::json!({
                                "use_ask": use_ask,
                            }));
                            return Ok(Value::String(serde_json::json!({
                                "behavior": "找不到相關技能規範，請直接回應使用者。",
                                "required_tools": []
                            }).to_string()));
                        }

                        let (first_id, first_title, _, _) = &skills[0];
                        let _ = app.emit("agent:skill_found", serde_json::json!({
                            "skill_id": first_id,
                            "skill_title": first_title,
                            "use_ask": use_ask,
                        }));

                        let required_tools: Vec<String> = skills.iter()
                            .flat_map(|(_, _, _, tools)| tools.iter().cloned())
                            .collect::<std::collections::HashSet<_>>()
                            .into_iter()
                            .collect();

                        let mut behavior = format!(
                            "# 技能搜尋結果（意圖：「{}」）\n\
                             以下是可能符合的技能規範，請根據使用者實際需求選擇最合適的一個執行：\n\n",
                            use_ask
                        );
                        for (idx, (_, title, beh, tool_names)) in skills.iter().enumerate() {
                            behavior.push_str(&format!(
                                "## 候選 {}：{}\n**執行方式**：{}\n**可用工具**：{}\n\n",
                                idx + 1, title, beh,
                                if tool_names.is_empty() { "（通用）".to_string() } else { tool_names.join("、") },
                            ));
                        }

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

    // notify_user — 發送通知到前端
    {
        let app = ctx.app.clone();
        registry.register(
            "notify_user".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let app = app.clone();
                    let title   = args["title"].as_str().unwrap_or("排程提醒").to_string();
                    let message = args["message"].as_str().unwrap_or("").to_string();
                    let task_id = args["task_id"].as_str().unwrap_or("").to_string();
                    Box::pin(async move {
                        let _ = app.emit("schedule:triggered", serde_json::json!({
                            "task_id":     task_id,
                            "title":       title,
                            "description": message,
                        }));
                        Ok(Value::String(format!("已通知使用者：{}", title)))
                    })
                }),
                rollback: None,
            },
        );
    }
}
