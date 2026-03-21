/// tools/agent.rs — Agent 工具、外部 AI、Web 搜尋
use std::sync::Arc;

use serde_json::Value;
use tauri::{Emitter, Manager};

use crate::commands::ai::{
    tool_list_recent_conversations, call_external_ai_via_db, vault_tools,
};
use crate::commands::knowledge_import::tool_web_search;
use crate::runtime::system_agent::{AgentRequest, NewSkillSpec};
use crate::runtime::tool_registry::ToolRegistry;
use crate::runtime::types::{ConfirmWriteFn, SummarizeFn, Tool};

use super::BuildCtx;

/// 建立子 agent 共用的 confirm_write 閉包
fn make_confirm_write(app: &tauri::AppHandle) -> ConfirmWriteFn {
    let app_state = app.state::<crate::state::AppState>();
    let write_tx = Arc::clone(&app_state.write_confirm_tx);
    Arc::new(move |_display: String| {
        let tx = Arc::clone(&write_tx);
        Box::pin(async move {
            let (ch_tx, ch_rx) = tokio::sync::oneshot::channel::<bool>();
            *tx.lock().await = Some(ch_tx);
            tokio::time::timeout(std::time::Duration::from_secs(60), ch_rx)
                .await.unwrap_or(Ok(false)).unwrap_or(false)
        })
    })
}

/// 建立子 agent 共用的 summarize_fn 閉包
async fn make_summarize_fn(app: &tauri::AppHandle, emb: &Option<String>) -> Option<SummarizeFn> {
    let Some(ref base_url) = emb else { return None; };
    let app_state = app.state::<crate::state::AppState>();
    let db = app_state.db.clone();
    let client = app_state.http_client.clone();
    let vid = app_state.get_vault_id().await.unwrap_or_default();
    let base = base_url.clone();
    Some(Arc::new(move |fp: String, uq: String| {
        let db = db.clone(); let client = client.clone();
        let vid = vid.clone(); let base = base.clone();
        Box::pin(async move {
            crate::commands::ai::parallel_chunk_summarize(&db, &vid, &fp, &uq, &client, &base).await
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
    }) as SummarizeFn)
}

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

    // call_external_ai — 呼叫前暫停，等待前端選擇搜尋方式
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        let app = ctx.app.clone();
        let emb = ctx.emb_url.clone();
        let tx = ctx.search_method_tx.clone();
        let cache = Arc::clone(&ctx.api_key_cache);
        let scache = Arc::clone(&ctx.settings_cache);
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
                    let scache = Arc::clone(&scache);
                    Box::pin(async move {
                        let _ = app.emit("agent:search_method_request", serde_json::json!({ "query": query }));
                        let method = {
                            let (ch_tx, ch_rx) = tokio::sync::oneshot::channel::<String>();
                            *tx.lock().await = Some(ch_tx);
                            tokio::time::timeout(std::time::Duration::from_secs(60), ch_rx)
                                .await
                                .unwrap_or(Ok("call_external_ai".to_string()))
                                .unwrap_or_else(|_| "call_external_ai".to_string())
                        };
                        let result = if method == "web_search" {
                            tool_web_search(&db, &vid, &query, &app, emb.as_deref()).await
                        } else {
                            let _ = app.emit("agent:web_refs", serde_json::json!([
                                {"path": "", "title": query, "excerpt": ""}
                            ]));
                            call_external_ai_via_db(&query, &db, &app, &cache, &scache).await
                        };
                        Ok(Value::String(result))
                    })
                }),
                rollback: None,
            },
        );
    }

    // web_search — 搜尋 DuckDuckGo Lite
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
        let app = ctx.app.clone();
        let emb = ctx.emb_url.clone();
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
                        Ok(Value::String(tool_web_search(&db, &vid, &query, &app, emb.as_deref()).await))
                    })
                }),
                rollback: None,
            },
        );
    }

    // list_recent_conversations — 讀取最近對話記錄
    {
        let db = ctx.vault_db.clone();
        let vid = ctx.vault_id.clone();
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

    // create_agent_skill — 透過 generate_skills_via_tool_call 建立技能規範（LLM 知悉所有工具）
    {
        let db  = ctx.vault_db.clone();
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
                    let db  = db.clone();
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
                        // 以 title + trigger + behavior 組成知識上下文
                        let context = format!(
                            "技能標題：{}\n觸發語境：{}\n行為描述：{}\n注入模式：{}",
                            title, trigger_hint, behavior, injection_mode
                        );
                        let skills = crate::commands::knowledge_import::generate_skills_via_tool_call(
                            &client, &base_url, &db, &vid, "", &context, emb.as_deref(), now_ms,
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

    // call_agent — 透過 SystemAgentService 路由，委派任務給對應的 agent
    {
        let app_ca   = ctx.app.clone();
        let llm_late = Arc::clone(&ctx.llm_fn_late);
        let reg_late = Arc::clone(&ctx.registry_late);
        let svc      = Arc::clone(&ctx.system_agent_svc);
        let cancel_ca = ctx.cancel.clone();
        let emb_ca   = ctx.emb_url.clone();

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
                    let emb     = emb_ca.clone();
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
                        let emit_fn: crate::runtime::types::EmitEventFn = {
                            let app = app.clone();
                            Arc::new(move |event: String, payload: Value| {
                                let _ = tauri::Emitter::emit(&app, &event, payload);
                            })
                        };
                        let confirm_write = make_confirm_write(&app);
                        let summarize_fn = make_summarize_fn(&app, &emb).await;
                        let result = svc.route(
                            AgentRequest {
                                caller_session_id: String::new(),
                                target,
                                task,
                                context,
                                parent_system: String::new(),
                                conversation_id: None,
                                vault_path: String::new(),
                            },
                            vault_tools(),
                            sub_registry,
                            llm_fn,
                            emit_fn,
                            cancel,
                            emb.as_deref(),
                            confirm_write,
                            None,
                            summarize_fn,
                        ).await;
                        Ok(Value::String(result))
                    })
                }),
                rollback: None,
            },
        );
    }

    // touch_agent — 自動語意匹配現有 agent 或建立新 agent，然後執行
    {
        let app_ta   = ctx.app.clone();
        let llm_late = Arc::clone(&ctx.llm_fn_late);
        let reg_late = Arc::clone(&ctx.registry_late);
        let svc_ta   = Arc::clone(&ctx.system_agent_svc);
        let cancel_ta = ctx.cancel.clone();
        let emb_ta   = ctx.emb_url.clone();

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
                    let llm_late = Arc::clone(&llm_late);
                    let reg_late = Arc::clone(&reg_late);
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
                        let emit_fn: crate::runtime::types::EmitEventFn = {
                            let app = app.clone();
                            Arc::new(move |event: String, payload: Value| {
                                let _ = tauri::Emitter::emit(&app, &event, payload);
                            })
                        };
                        let def = svc.touch_agent(
                            name.clone(), description, trigger,
                            tool_names, String::new(), vec![], 5,
                            emb.as_deref(), emit_fn.clone(), &task,
                        ).await;
                        let target = match def {
                            Ok(ref d) => d.def_id.clone(),
                            Err(_) => name,
                        };
                        let confirm_write = make_confirm_write(&app);
                        let summarize_fn = make_summarize_fn(&app, &emb).await;
                        let result = svc.route(
                            AgentRequest {
                                caller_session_id: String::new(),
                                target,
                                task,
                                context,
                                parent_system: String::new(),
                                conversation_id: None,
                                vault_path: String::new(),
                            },
                            crate::commands::ai::vault_tools(),
                            sub_registry,
                            llm_fn,
                            emit_fn,
                            cancel,
                            emb.as_deref(),
                            confirm_write,
                            None,
                            summarize_fn,
                        ).await;
                        Ok(Value::String(result))
                    })
                }),
                rollback: None,
            },
        );
    }

    // create_agent — 動態建立 agent definition + skills
    {
        let svc_ca = Arc::clone(&ctx.system_agent_svc);
        let emb_ca = ctx.emb_url.clone();
        let app_ca = ctx.app.clone();

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
                            .map(|a| a.iter().filter_map(|v| serde_json::from_value::<NewSkillSpec>(v.clone()).ok()).collect())
                            .unwrap_or_default();

                        let emit_fn: crate::runtime::types::EmitEventFn = {
                            let app = app.clone();
                            Arc::new(move |event: String, payload: Value| {
                                let _ = tauri::Emitter::emit(&app, &event, payload);
                            })
                        };

                        match svc.create_agent(
                            name.clone(), description, trigger, tool_names, system_prompt,
                            skills, max_rounds, emb.as_deref(), emit_fn,
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
        let db_la  = ctx.vault_db.clone();
        let vid_la = ctx.vault_id.clone();

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
                                d.name, d.def_id, d.kind, d.description,
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
}
