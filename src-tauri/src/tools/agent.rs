/// tools/agent.rs — Agent 工具、外部 AI、Web 搜尋
use std::sync::Arc;

use serde_json::Value;
use tauri::{Emitter, Manager};

use crate::commands::ai::{
    tool_list_recent_conversations,
};
use crate::commands::knowledge_import::tool_web_search;
use crate::runtime::tool_registry::ToolRegistry;
use crate::runtime::types::Tool;

use super::BuildCtx;

pub fn register(registry: &mut ToolRegistry, ctx: &BuildCtx) {
    // open_note — 發送 ui:open_note 事件讓前端開啟筆記
    {
        let app = ctx.app.clone();
        let vp = ctx.vault_path.clone();
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
                        let abs = std::path::PathBuf::from(&vp).join(&rel);
                        if abs.exists() {
                            let abs_str = abs.to_string_lossy().to_string();
                            let _ = app.emit("ui:open_note", &abs_str);
                            return Ok(Value::String(format!("✅ 已打開筆記：{}", rel)));
                        }
                        let filename = std::path::Path::new(&rel)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| rel.clone());
                        let mut found: Option<String> = None;
                        if let Ok(walker) = std::fs::read_dir(&vp) {
                            for entry in walker.flatten() {
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

    // web_search — 搜尋網路（Brave Search）
    {
        let http_client = ctx.http_client.clone();
        let auth_token = ctx.auth_token.clone();
        let vid = ctx.vault_id.clone();
        let app = ctx.app.clone();
        let emb = ctx.emb_url.clone();
        registry.register(
            "web_search".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let query = args["query"].as_str().unwrap_or("").to_string();
                    let http_client = http_client.clone();
                    let auth_token = auth_token.clone();
                    let vid = vid.clone();
                    let app = app.clone();
                    let emb = emb.clone();
                    Box::pin(async move {
                        Ok(Value::String(tool_web_search(&http_client, &auth_token, &vid, &query, &app, emb.as_deref()).await))
                    })
                }),
                rollback: None,
            },
        );
    }

    // list_recent_conversations — 讀取最近對話記錄
    {
        let http_client = ctx.http_client.clone();
        let auth_token = ctx.auth_token.clone();
        registry.register(
            "list_recent_conversations".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let limit = args["limit"].as_u64().unwrap_or(10) as usize;
                    let http_client = http_client.clone();
                    let auth_token = auth_token.clone();
                    Box::pin(async move {
                        Ok(Value::String(tool_list_recent_conversations(&http_client, &auth_token, limit).await))
                    })
                }),
                rollback: None,
            },
        );
    }

    // create_agent_skill — 透過 generate_skills_via_tool_call 建立技能規範（LLM 知悉所有工具）
    {
        let vid = ctx.vault_id.clone();
        let emb = ctx.emb_url.clone();
        let app = ctx.app.clone();
        registry.register(
            "create_agent_skill".into(),
            Tool {
                execute: Arc::new(move |args: Value| {
                    let title          = args["title"].as_str().unwrap_or("").to_string();
                    let trigger_hint   = args["trigger"].as_str().unwrap_or("").to_string();
                    let behavior       = args["behavior"].as_str().unwrap_or("").to_string();
                    let injection_mode = args["injection_mode"].as_str().unwrap_or("passive").to_string();
                    let vid = vid.clone();
                    let emb = emb.clone();
                    let app = app.clone();
                    Box::pin(async move {
                        // 取得 llama server URL
                        let app_state = app.state::<crate::state::AppState>();
                        let port = *app_state.llama_actual_port.lock().await;
                        let Some(base_url) = port.map(|p| format!("http://127.0.0.1:{}", p)) else {
                            return Err("LLM server 未啟動，無法建立技能規範".to_string());
                        };
                        let client = reqwest::Client::new();
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        let context = format!(
                            "技能標題：{}\n觸發語境：{}\n行為描述：{}\n注入模式：{}",
                            title, trigger_hint, behavior, injection_mode
                        );
                        let skills = crate::commands::knowledge_import::generate_skills_via_tool_call(
                            &client, &base_url, None, &vid, "", &context, emb.as_deref(), now_ms,
                        ).await;
                        if skills.is_empty() {
                            Ok(Value::String("技能建立失敗：LLM 未能生成有效技能規範".to_string()))
                        } else {
                            Ok(Value::String(format!(
                                "✅ 已建立 {} 個技能規範：{}",
                                skills.len(),
                                skills.iter().map(|s| s.title.as_str()).collect::<Vec<_>>().join("、")
                            )))
                        }
                    })
                }),
                rollback: None,
            },
        );
    }

    // list_available_agents — daemon GET /vaults/:vid/agents
    {
        let http_client = ctx.http_client.clone();
        let auth_token = ctx.auth_token.clone();
        let vid_la = ctx.vault_id.clone();

        registry.register(
            "list_available_agents".into(),
            Tool {
                execute: Arc::new(move |_args: Value| {
                    let http_client = http_client.clone();
                    let auth_token = auth_token.clone();
                    let vid = vid_la.clone();
                    Box::pin(async move {
                        let tok = if auth_token.is_empty() { None } else { Some(auth_token.as_str()) };
                        let result: serde_json::Value = crate::api_client::daemon_get(
                            &http_client,
                            &format!("/vaults/{}/agents", urlencoding::encode(&vid)),
                            tok,
                        ).await.unwrap_or_else(|_| serde_json::json!([]));
                        let agents = result.as_array().cloned().unwrap_or_default();
                        let lines: Vec<String> = agents.iter().map(|d| {
                            let name = d["name"].as_str().unwrap_or("(無名稱)");
                            let def_id = d["def_id"].as_str().unwrap_or("");
                            let kind = d["kind"].as_str().unwrap_or("custom");
                            let desc = d["description"].as_str().unwrap_or("");
                            let tools = d["tool_names"].as_array()
                                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                                .unwrap_or_default();
                            format!("- **{}** (id: `{}`, kind: {}) — {} [tools: {}]", name, def_id, kind, desc, tools)
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
}
