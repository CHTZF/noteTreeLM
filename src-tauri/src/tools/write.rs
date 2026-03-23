/// tools/write.rs — 寫入工具（含 rollback）與副作用工具
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tauri::Emitter;
use tokio::sync::Mutex;

use crate::commands::ai::{resolve_vault_path, tool_create_note, tool_update_note, tool_create_folder};
use crate::runtime::memory_agent::add_memory_rule_to_db;
use crate::runtime::tool_registry::ToolRegistry;
use crate::runtime::types::Tool;

use super::BuildCtx;

pub fn register(registry: &mut ToolRegistry, ctx: &BuildCtx) {
    // create_note — rollback: 刪除剛建立的檔案
    {
        let vp_exec = ctx.vault_path.clone();
        let vp_rb = ctx.vault_path.clone();
        let db_cn = ctx.vault_db.clone();
        let vid_cn = ctx.vault_id.clone();
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
    {
        let vp_exec = ctx.vault_path.clone();
        let vp_rb = ctx.vault_path.clone();
        let db_un = ctx.vault_db.clone();
        let vid_un = ctx.vault_id.clone();
        let backups: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
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
                        let original = tokio::fs::read_to_string(&abs_path).await.unwrap_or_default();
                        backups.lock().await.insert(path.clone(), original);
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

    // create_folder — rollback: 移除剛建立的資料夾（僅在空的時候才會成功）
    {
        let vp_exec = ctx.vault_path.clone();
        let vp_rb = ctx.vault_path.clone();
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
                            let _ = tokio::fs::remove_dir(&abs_path).await;
                        }
                        Ok(Value::Null)
                    })
                })),
            },
        );
    }

    // add_memory_rule — 讓 LLM 學習新的時間表達式規則
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
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

    // append_to_note — 在既有筆記末尾追加內容
    {
        let vp = ctx.vault_path.clone();
        let app_an = ctx.app.clone();
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

    // delete_note — 刪除筆記並清除 DB 記錄
    {
        let vp = ctx.vault_path.clone();
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
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
                        let direct = std::path::PathBuf::from(&vp).join(&rel);
                        let abs_path = if direct.exists() {
                            direct.to_string_lossy().to_string()
                        } else {
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

    // delete_folder — 刪除資料夾及其所有內容
    {
        let vp = ctx.vault_path.clone();
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
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

                        let prefix = if path.ends_with('/') { path.clone() } else { format!("{}/", path) };
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

    // move_note — 移動筆記並更新 DB 路徑
    {
        let vp = ctx.vault_path.clone();
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
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

    // show_toast — 顯示 UI 通知
    {
        let app_st = ctx.app.clone();
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

    // ui_action — 觸發 UI 動作
    {
        let app_ua = ctx.app.clone();
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

    // compress_to_knowledge — 儲存重要知識/洞見到 knowledge/ 資料夾
    {
        let vp = ctx.vault_path.clone();
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "compress_to_knowledge".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let title = args["title"].as_str().unwrap_or("").to_string();
                    let content = args["content"].as_str().unwrap_or("").to_string();
                    let tags: Vec<String> = args["tags"].as_array()
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    let vp = vp.clone();
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        if title.is_empty() { return Err("title 為必填".to_string()); }
                        if content.is_empty() { return Err("content 為必填".to_string()); }
                        let now = chrono::Local::now();
                        let date_str = now.format("%Y-%m-%d").to_string();
                        let tags_yaml = if tags.is_empty() {
                            "  - knowledge".to_string()
                        } else {
                            tags.iter().map(|t| format!("  - {}", t)).collect::<Vec<_>>().join("\n")
                        };
                        let full_content = format!(
                            "---\ntitle: {}\ncreated: {}\ntags:\n{}\nsource: ai_compressed\n---\n\n{}\n",
                            title, now.to_rfc3339(), tags_yaml, content
                        );
                        let safe_title: String = title.chars()
                            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                            .collect();
                        let path = format!("knowledge/{}_{}.md", date_str, safe_title);
                        let result = crate::commands::ai::tool_create_note(&path, &full_content, &vp, Some((db, vid))).await;
                        if result.contains("失敗") { Err(result) } else { Ok(Value::String(format!("✅ 已儲存知識至 {}", path))) }
                    })
                }),
                rollback: None,
            },
        );
    }

    // update_note_frontmatter — 局部更新 YAML frontmatter 欄位（不覆蓋正文）
    {
        let vp = ctx.vault_path.clone();
        registry.register(
            "update_note_frontmatter".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let path = args["path"].as_str().unwrap_or("").to_string();
                    let fields = args["fields"].clone();
                    let vp = vp.clone();
                    Box::pin(async move {
                        if path.is_empty() { return Err("path 為必填".to_string()); }
                        if !fields.is_object() { return Err("fields 必須為物件（鍵值對）".to_string()); }
                        let rel = if path.ends_with(".md") { path.clone() } else { format!("{}.md", path) };
                        let abs = std::path::PathBuf::from(&vp).join(&rel);
                        let original = tokio::fs::read_to_string(&abs).await
                            .map_err(|e| format!("讀取失敗：{}", e))?;
                        let new_content = if original.starts_with("---") {
                            let after_first = &original[3..];
                            if let Some(end_pos) = after_first.find("\n---") {
                                let fm_content = &after_first[..end_pos];
                                let body = &after_first[end_pos + 4..];
                                let mut lines: Vec<String> = fm_content.lines().map(|l| l.to_string()).collect();
                                if let Some(obj) = fields.as_object() {
                                    for (key, val) in obj {
                                        let val_str = match val {
                                            serde_json::Value::String(s) => s.clone(),
                                            serde_json::Value::Array(arr) => {
                                                let items: Vec<String> = arr.iter()
                                                    .filter_map(|v| v.as_str().map(String::from))
                                                    .collect();
                                                format!("[{}]", items.join(", "))
                                            }
                                            v => v.to_string(),
                                        };
                                        let key_prefix = format!("{}:", key);
                                        let mut found = false;
                                        for line in &mut lines {
                                            if line.starts_with(&key_prefix) {
                                                *line = format!("{}: {}", key, val_str);
                                                found = true;
                                                break;
                                            }
                                        }
                                        if !found { lines.push(format!("{}: {}", key, val_str)); }
                                    }
                                }
                                format!("---\n{}\n---{}", lines.join("\n"), body)
                            } else { original }
                        } else {
                            let mut fm = vec!["---".to_string()];
                            if let Some(obj) = fields.as_object() {
                                for (key, val) in obj {
                                    let vs = match val { serde_json::Value::String(s) => s.clone(), v => v.to_string() };
                                    fm.push(format!("{}: {}", key, vs));
                                }
                            }
                            fm.push("---".to_string());
                            fm.push(String::new());
                            fm.push(original);
                            fm.join("\n")
                        };
                        tokio::fs::write(&abs, &new_content).await
                            .map_err(|e| format!("寫入失敗：{}", e))?;
                        Ok(Value::String(format!("✅ 已更新 {} 的 frontmatter 欄位", rel)))
                    })
                }),
                rollback: None,
            },
        );
    }

    // link_notes — 在筆記 A 中插入 [[筆記B]] 反向連結
    {
        let vp = ctx.vault_path.clone();
        registry.register(
            "link_notes".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let from_path = args["from_path"].as_str().unwrap_or("").to_string();
                    let to_path = args["to_path"].as_str().unwrap_or("").to_string();
                    let section = args["section"].as_str().unwrap_or("").to_string();
                    let vp = vp.clone();
                    Box::pin(async move {
                        if from_path.is_empty() || to_path.is_empty() {
                            return Err("from_path 和 to_path 為必填".to_string());
                        }
                        let from_rel = if from_path.ends_with(".md") { from_path.clone() } else { format!("{}.md", from_path) };
                        let abs = std::path::PathBuf::from(&vp).join(&from_rel);
                        let content = tokio::fs::read_to_string(&abs).await
                            .map_err(|e| format!("讀取失敗：{}", e))?;
                        // Build link text: [[stem_name]]
                        let to_stem = std::path::Path::new(&to_path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| to_path.trim_end_matches(".md").to_string());
                        let link = format!("[[{}]]", to_stem);
                        // Check if already linked
                        if content.contains(&link) {
                            return Ok(Value::String(format!("「{}」已包含 {} 連結，不需重複加入", from_rel, link)));
                        }
                        // Insert into specified section or append Related section
                        let new_content = if !section.is_empty() {
                            // Find section heading and insert after it
                            let heading = format!("## {}", section);
                            if let Some(pos) = content.find(&heading) {
                                let after_heading = pos + heading.len();
                                let next_section = content[after_heading..].find("\n## ").map(|p| after_heading + p);
                                let insert_at = next_section.unwrap_or(content.len());
                                let before = content[..insert_at].trim_end();
                                let after = &content[insert_at..];
                                format!("{}\n{}\n{}", before, link, after)
                            } else {
                                // Section not found, append Related section
                                format!("{}\n\n## {}\n{}\n", content.trim_end(), section, link)
                            }
                        } else {
                            // Check for existing Related / Links section
                            let rel_heading = if content.contains("## Related") { "## Related" }
                                else if content.contains("## Links") { "## Links" }
                                else if content.contains("## 相關") { "## 相關" }
                                else { "" };
                            if !rel_heading.is_empty() {
                                if let Some(pos) = content.find(rel_heading) {
                                    let after = pos + rel_heading.len();
                                    let next_section = content[after..].find("\n## ").map(|p| after + p);
                                    let insert_at = next_section.unwrap_or(content.len());
                                    let before = content[..insert_at].trim_end();
                                    let rest = &content[insert_at..];
                                    format!("{}\n{}\n{}", before, link, rest)
                                } else {
                                    format!("{}\n\n## Related\n{}\n", content.trim_end(), link)
                                }
                            } else {
                                format!("{}\n\n## Related\n{}\n", content.trim_end(), link)
                            }
                        };
                        tokio::fs::write(&abs, &new_content).await
                            .map_err(|e| format!("寫入失敗：{}", e))?;
                        Ok(Value::String(format!("✅ 已在「{}」加入連結 {}", from_rel, link)))
                    })
                }),
                rollback: None,
            },
        );
    }

    // generate_moc — 為資料夾生成 Map of Contents（索引筆記）
    {
        let vp = ctx.vault_path.clone();
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "generate_moc".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let folder = args["folder"].as_str().unwrap_or("").to_string();
                    let output_path = args["output_path"].as_str().unwrap_or("").to_string();
                    let vp = vp.clone();
                    let db = db.clone();
                    let vid = vid.clone();
                    Box::pin(async move {
                        if folder.is_empty() { return Err("請提供資料夾路徑".to_string()); }
                        let prefix = if folder.ends_with('/') { folder.clone() } else { format!("{}/", folder) };
                        #[derive(serde::Deserialize)]
                        struct NoteRow { path: String, title: Option<String> }
                        let mut resp = db.query(
                            "SELECT path, title FROM notes \
                             WHERE vault_id = $vid AND string::starts_with(path, $prefix) \
                             ORDER BY path ASC LIMIT 200"
                        )
                        .bind(("vid", vid.clone()))
                        .bind(("prefix", prefix.clone()))
                        .await
                        .map_err(|e| e.to_string())?;
                        let rows: Vec<NoteRow> = resp.take(0).unwrap_or_default();
                        if rows.is_empty() {
                            return Ok(Value::String(format!("資料夾「{}」中沒有筆記，無法生成 MOC", folder)));
                        }
                        // Group by sub-folder
                        let folder_name = folder.split('/').last().unwrap_or(&folder);
                        let now = chrono::Local::now();
                        let mut moc = format!(
                            "---\ntitle: {folder_name} Index\ncreated: {}\ntags:\n  - moc\n  - index\n---\n\n# {folder_name}\n\n> 自動生成的目錄筆記（{}）\n\n",
                            now.to_rfc3339(), now.format("%Y-%m-%d %H:%M")
                        );
                        // Group notes by immediate sub-folder
                        let mut groups: std::collections::BTreeMap<String, Vec<&NoteRow>> = std::collections::BTreeMap::new();
                        for row in &rows {
                            let rel = row.path.strip_prefix(&prefix).unwrap_or(&row.path);
                            let sub = if rel.contains('/') {
                                rel.split('/').next().unwrap_or("").to_string()
                            } else {
                                String::new() // top-level
                            };
                            groups.entry(sub).or_default().push(row);
                        }
                        // Top-level notes first
                        if let Some(top) = groups.remove("") {
                            moc.push_str("## 筆記\n\n");
                            for row in top {
                                let stem = std::path::Path::new(&row.path)
                                    .file_stem().map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                let title = row.title.as_deref().unwrap_or(&stem);
                                moc.push_str(&format!("- [[{}]] — {}\n", stem, title));
                            }
                            moc.push('\n');
                        }
                        // Sub-folders
                        for (sub, notes) in &groups {
                            moc.push_str(&format!("## {}\n\n", sub));
                            for row in notes {
                                let stem = std::path::Path::new(&row.path)
                                    .file_stem().map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                let title = row.title.as_deref().unwrap_or(&stem);
                                moc.push_str(&format!("- [[{}]] — {}\n", stem, title));
                            }
                            moc.push('\n');
                        }
                        let out_path = if output_path.is_empty() {
                            format!("{}/index.md", folder.trim_end_matches('/'))
                        } else {
                            if output_path.ends_with(".md") { output_path.clone() } else { format!("{}.md", output_path) }
                        };
                        let result = crate::commands::ai::tool_create_note(&out_path, &moc, &vp, Some((db, vid))).await;
                        if result.contains("失敗") { Err(result) } else {
                            Ok(Value::String(format!("✅ 已生成 MOC：{} （共 {} 篇筆記）", out_path, rows.len())))
                        }
                    })
                }),
                rollback: None,
            },
        );
    }

    // schedule_task — 插入 scheduled_tasks 表
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        registry.register(
            "schedule_task".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let description = args["description"].as_str().unwrap_or("").to_string();
                    let run_at_str = args["run_at"].as_str().unwrap_or("").to_string();
                    let repeat_interval_secs = args["repeat_interval_seconds"].as_i64().unwrap_or(0);
                    let agent_type = args["agent_type"].as_str().map(String::from);
                    let agent_prompt = args["agent_prompt"].as_str().map(String::from);
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
                            "INSERT INTO scheduled_tasks \
                             (task_id, vault_id, description, agent_type, agent_prompt, run_at_ts, repeat_interval_secs, status, created_at) \
                             VALUES ($tid, $vid, $desc, $atype, $aprompt, $ts, $interval, 'pending', $now)"
                        )
                        .bind(("tid", task_id.clone()))
                        .bind(("vid", vid.clone()))
                        .bind(("desc", description.clone()))
                        .bind(("atype", agent_type.clone()))
                        .bind(("aprompt", agent_prompt.clone()))
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
                        let agent_info = agent_type
                            .map(|t| format!("，呼叫 agent: {}", t))
                            .unwrap_or_default();

                        Ok(Value::String(format!(
                            "已排程「{}」於 {}{}{}（task_id: {}）",
                            description, run_at_str, repeat_info, agent_info, task_id
                        )))
                    })
                }),
                rollback: None,
            },
        );
    }
}
