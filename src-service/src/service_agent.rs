/// service_agent.rs
///
/// Service-side sub-agent runner for scheduled tasks.
///
/// Flow:
///   1. Look up agent_definitions by name + account_id
///   2. Build tool registry (service-side tools only)
///   3. Run agentic LLM loop (max_rounds)
///   4. Emit SSE when done

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use crate::api_state::ApiState;
use crate::db::SurrealDb;
use crate::state::AgentSession;

const MAX_ROUNDS: usize = 20;

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn execute_scheduled_task(
    state: ApiState,
    task_id: String,
    vault_id: String,
    account_id: String,
    agent_def_name: Option<String>,
    agent_prompt: Option<String>,
    description: String,
) {
    let agent_name = match agent_def_name {
        Some(ref n) if !n.is_empty() => n.clone(),
        _ => {
            // No agent — just emit notification
            state.daemon.emit("schedule:triggered", json!({
                "task_id": task_id,
                "vault_id": vault_id,
                "description": description,
            }));
            return;
        }
    };

    let llm_url = state.daemon.llm_url.read().await.clone();
    let Some(llm_url) = llm_url else {
        tracing::warn!("[scheduler] llm_url not available, skipping task {}", task_id);
        return;
    };

    // Look up agent definition
    let agent_def = match load_agent_def(&state.db, &agent_name, &account_id).await {
        Some(a) => a,
        None => {
            tracing::warn!("[scheduler] agent '{}' not found for account '{}'", agent_name, account_id);
            return;
        }
    };

    let system_prompt = agent_def["system_prompt"].as_str().unwrap_or("").to_string();
    let max_rounds = agent_def["max_rounds"].as_i64().unwrap_or(MAX_ROUNDS as i64) as usize;
    let tool_names: Vec<String> = agent_def["tool_names"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let initial_msg = agent_prompt.unwrap_or_else(|| description.clone());

    tracing::info!(
        "[scheduler] running agent '{}' for task {} (tools: {:?})",
        agent_name, task_id, tool_names
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap_or_default();

    let embedding_url = state.daemon.embedding_url.read().await.clone();

    let result = run_agent_loop(
        &client,
        &llm_url,
        &state.db,
        &vault_id,
        &account_id,
        &embedding_url,
        &system_prompt,
        &initial_msg,
        &tool_names,
        max_rounds,
    ).await;

    state.daemon.emit("schedule:completed", json!({
        "task_id": task_id,
        "agent": agent_name,
        "vault_id": vault_id,
        "description": description,
        "summary": result,
    }));
}

// ── Agent loop ────────────────────────────────────────────────────────────────

async fn run_agent_loop(
    client: &reqwest::Client,
    llm_url: &str,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    embedding_url: &Option<String>,
    system_prompt: &str,
    initial_msg: &str,
    tool_names: &[String],
    max_rounds: usize,
) -> String {
    let tools_schema = build_tools_schema(tool_names);
    let mut messages: Vec<Value> = vec![
        json!({ "role": "system", "content": system_prompt }),
        json!({ "role": "user",   "content": initial_msg }),
    ];

    let mut last_text = String::new();

    for _round in 0..max_rounds {
        let body = if tools_schema.is_empty() {
            json!({
                "messages": messages,
                "stream": false,
                "temperature": 0.3,
                "max_tokens": 2048,
            })
        } else {
            json!({
                "messages": messages,
                "tools": tools_schema,
                "tool_choice": "auto",
                "stream": false,
                "temperature": 0.3,
                "max_tokens": 2048,
            })
        };

        let Ok(resp) = client
            .post(format!("{}/v1/chat/completions", llm_url))
            .json(&body)
            .send().await
        else { break };

        let Ok(resp_json) = resp.json::<Value>().await else { break };

        let choice = &resp_json["choices"][0];
        let finish_reason = choice["finish_reason"].as_str().unwrap_or("stop");
        let message = &choice["message"];
        let text = message["content"].as_str().unwrap_or("").to_string();

        if !text.is_empty() {
            last_text = text.clone();
        }

        // Check for tool calls
        if finish_reason == "tool_calls" || message["tool_calls"].is_array() {
            let tool_calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
            if tool_calls.is_empty() { break; }

            // Add assistant message with tool_calls
            messages.push(message.clone());

            // Execute each tool call
            for tc in &tool_calls {
                let tc_id = tc["id"].as_str().unwrap_or("").to_string();
                let fn_name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                let fn_args: Value = tc["function"]["arguments"]
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(json!({}));

                let result = dispatch_tool(client, llm_url, db, vault_id, account_id, embedding_url, &fn_name, &fn_args).await;
                let result_str = match &result {
                    Ok(v) => serde_json::to_string(v).unwrap_or_default(),
                    Err(e) => format!("ERROR: {}", e),
                };

                tracing::debug!("[scheduler] tool {} → {}", fn_name, &result_str[..result_str.len().min(200)]);

                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tc_id,
                    "content": result_str,
                }));
            }
        } else {
            // No tool calls — done
            break;
        }
    }

    last_text
}

// ── Tool dispatch ─────────────────────────────────────────────────────────────

async fn dispatch_tool(
    client: &reqwest::Client,
    llm_url: &str,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    embedding_url: &Option<String>,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    match name {
        "get_unprocessed_conversations" => {
            let limit = args["limit"].as_i64().unwrap_or(20);
            get_unprocessed_conversations(db, vault_id, account_id, limit).await
        }
        "get_conversation_content" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            let skip_count = args["skip_count"].as_i64().unwrap_or(0);
            let char_limit = args["char_limit"].as_i64().unwrap_or(500);
            get_conversation_content(db, &conv_id, skip_count, char_limit).await
        }
        "save_memory_facts" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            let facts = args["facts"].as_array().cloned().unwrap_or_default();
            save_memory_facts(client, db, vault_id, account_id, &conv_id, facts, embedding_url).await
        }
        "mark_conversation_processed" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            mark_conversation_processed(db, &conv_id).await
        }
        "condense_memory_facts" => {
            let category = args["category"].as_str().map(String::from);
            condense_memory_facts(client, llm_url, db, vault_id, account_id, category, embedding_url).await
        }
        _ => Err(format!("unknown tool: {}", name)),
    }
}

// ── Tool implementations ──────────────────────────────────────────────────────

async fn get_unprocessed_conversations(
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    limit: i64,
) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: String,
        title: String,
        updated_at: i64,
        messages_json: Option<String>,
        memory_processed_msg_count: Option<i64>,
    }

    let mut resp = db
        .query("SELECT record::id(id) AS id, title, updated_at, messages_json, memory_processed_msg_count \
                FROM conversations \
                WHERE vault_id = $vid AND account_id = $aid \
                AND (memory_processed_at IS NONE OR memory_processed_at < updated_at) \
                ORDER BY updated_at DESC LIMIT $lim")
        .bind(("vid", vault_id.to_string()))
        .bind(("aid", account_id.to_string()))
        .bind(("lim", limit))
        .await
        .map_err(|e| e.to_string())?;

    let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;

    let items: Vec<Value> = rows.into_iter().map(|r| {
        let msgs: Value = r.messages_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(json!([]));
        let message_count = msgs.as_array().map(|a| a.len()).unwrap_or(0);
        let processed_msg_count = r.memory_processed_msg_count.unwrap_or(0);
        let preview = msgs.as_array()
            .and_then(|a| a.iter().rev().find(|m| m["role"].as_str() == Some("user")))
            .and_then(|m| m["content"].as_str())
            .map(|s| s.chars().take(100).collect::<String>())
            .unwrap_or_default();
        json!({
            "conversation_id": r.id,
            "title": r.title,
            "updated_at": r.updated_at,
            "message_count": message_count,
            "processed_msg_count": processed_msg_count,
            "new_message_count": (message_count as i64 - processed_msg_count).max(0),
            "preview": preview,
        })
    }).collect();

    Ok(json!(items))
}

async fn get_conversation_content(
    db: &SurrealDb,
    conv_id: &str,
    skip_count: i64,
    char_limit: i64,
) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Row { messages_json: Option<String> }

    let mut resp = db
        .query("SELECT messages_json FROM conversations WHERE record::id(id) = $cid LIMIT 1")
        .bind(("cid", conv_id.to_string()))
        .await
        .map_err(|e| e.to_string())?;

    let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;
    let messages_json = rows.into_iter().next()
        .and_then(|r| r.messages_json)
        .unwrap_or_else(|| "[]".to_string());

    let msgs: Value = serde_json::from_str(&messages_json).unwrap_or(json!([]));
    let arr = msgs.as_array().cloned().unwrap_or_default();
    let skip = (skip_count as usize).min(arr.len());

    let limit = (char_limit.max(100).min(8000)) as usize;
    let text = arr[skip..].iter()
        .filter(|m| matches!(m["role"].as_str(), Some("user") | Some("assistant")))
        .map(|m| {
            let role = m["role"].as_str().unwrap_or("user");
            let content: String = m["content"].as_str().unwrap_or("").chars().take(limit).collect();
            format!("[{}]: {}", role, content)
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    Ok(Value::String(text))
}

async fn save_memory_facts(
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    conv_id: &str,
    facts: Vec<Value>,
    embedding_url: &Option<String>,
) -> Result<Value, String> {
    let valid_cats = ["personal", "preference", "project", "rule", "general"];
    let now = chrono::Utc::now().timestamp();
    let mut count = 0u32;

    for fact in &facts {
        let content = match fact.as_str().or_else(|| fact["content"].as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let category = fact["category"].as_str()
            .map(|c| if valid_cats.contains(&c) { c } else { "general" })
            .unwrap_or("general")
            .to_string();

        // expires_at: personal/rule → 365 days, others → 90 days
        let days = if category == "personal" || category == "rule" { 365i64 } else { 90 };
        let expires_at = now + days * 86400;

        // Dedup: skip if a non-expired fact with same prefix (first 40 chars) already exists
        let prefix: String = content.chars().take(40).collect::<String>().to_lowercase();
        let mut check = db
            .query("SELECT fact_id FROM memory_facts WHERE vault_id = $vid AND string::startsWith(string::lowercase(content), $prefix) AND expires_at > $now LIMIT 1")
            .bind(("vid", vault_id.to_string()))
            .bind(("prefix", prefix))
            .bind(("now", now))
            .await
            .map_err(|e| e.to_string())?;
        let existing: Vec<Value> = check.take(0).map_err(|e| e.to_string())?;
        if !existing.is_empty() {
            // Refresh expires_at
            let fid = existing[0]["fact_id"].as_str().unwrap_or("").to_string();
            let _ = db.query("UPDATE memory_facts SET expires_at = $exp WHERE fact_id = $fid")
                .bind(("exp", expires_at))
                .bind(("fid", fid))
                .await;
            continue;
        }

        // Compute embedding (stored as native array, not JSON string)
        let embedding_vec: Option<Vec<f32>> =
            crate::embedder::embed_text(client, embedding_url, &content).await;

        let fact_id = uuid::Uuid::new_v4().to_string();
        let _ = db
            .query("INSERT INTO memory_facts (fact_id, vault_id, account_id, conv_id, content, category, expires_at, created_at, embedding) \
                    VALUES ($fid, $vid, $aid, $cid, $content, $cat, $exp, $now, $emb)")
            .bind(("fid", fact_id))
            .bind(("vid", vault_id.to_string()))
            .bind(("aid", account_id.to_string()))
            .bind(("cid", conv_id.to_string()))
            .bind(("content", content))
            .bind(("cat", category))
            .bind(("exp", expires_at))
            .bind(("now", now))
            .bind(("emb", embedding_vec))
            .await;
        count += 1;
    }

    Ok(json!({ "facts_saved": count }))
}

async fn mark_conversation_processed(
    db: &SurrealDb,
    conv_id: &str,
) -> Result<Value, String> {
    // Read current message count so scheduler knows the watermark
    #[derive(serde::Deserialize)]
    struct Row { messages_json: Option<String> }
    let msg_count = db
        .query("SELECT messages_json FROM conversations WHERE record::id(id) = $cid LIMIT 1")
        .bind(("cid", conv_id.to_string()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .and_then(|r| r.messages_json)
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .map(|arr| arr.len() as i64)
        .unwrap_or(0);

    let now = chrono::Utc::now().timestamp();
    db.query("UPDATE conversations SET memory_processed_at = $now, memory_processed_msg_count = $count WHERE record::id(id) = $cid")
        .bind(("cid", conv_id.to_string()))
        .bind(("now", now))
        .bind(("count", msg_count))
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true, "processed_msg_count": msg_count }))
}

async fn condense_memory_facts(
    client: &reqwest::Client,
    llm_url: &str,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    category: Option<String>,
    embedding_url: &Option<String>,
) -> Result<Value, String> {
    const CONDENSE_THRESHOLD: usize = 8;
    let all_cats = ["personal", "preference", "project", "rule", "general"];
    let cats: Vec<&str> = match &category {
        Some(c) if all_cats.contains(&c.as_str()) => vec![c.as_str()],
        _ => all_cats.to_vec(),
    };

    let now = chrono::Utc::now().timestamp();
    let mut condensed = 0u32;

    for cat in cats {
        #[derive(serde::Deserialize)]
        struct FactRow { fact_id: String, content: String }

        let Ok(mut resp) = db
            .query("SELECT fact_id, content FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND category = $cat AND expires_at > $now ORDER BY created_at DESC LIMIT 50")
            .bind(("vid", vault_id.to_string()))
            .bind(("aid", account_id.to_string()))
            .bind(("cat", cat.to_string()))
            .bind(("now", now))
            .await
        else { continue };

        let facts: Vec<FactRow> = resp.take(0).unwrap_or_default();
        if facts.len() < CONDENSE_THRESHOLD { continue }

        let source_ids: Vec<String> = facts.iter().map(|f| f.fact_id.clone()).collect();
        let facts_text = facts.iter().enumerate()
            .map(|(i, f)| format!("{}. {}", i + 1, f.content))
            .collect::<Vec<_>>().join("\n");

        let body = json!({
            "messages": [
                { "role": "system", "content": format!(
                    "你是記憶壓縮系統。以下是類別「{}」的記憶事實，請整合成 2-4 條精煉摘要，\
                     保留最重要資訊，去除重複和過時內容。每條以「- 」開頭，不加編號，不加解釋。", cat
                )},
                { "role": "user", "content": facts_text },
            ],
            "stream": false, "temperature": 0.1, "max_tokens": 300,
        });

        let Ok(resp) = client.post(format!("{}/v1/chat/completions", llm_url))
            .json(&body).send().await
        else { continue };
        let Ok(resp_json) = resp.json::<Value>().await else { continue };
        let summary = match resp_json["choices"][0]["message"]["content"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };

        // Concurrent condensation guard: verify at least one source fact still exists
        // before proceeding. If another agent already condensed this batch, skip.
        let still_exists: Vec<Value> = db
            .query("SELECT fact_id FROM memory_facts WHERE fact_id = $fid LIMIT 1")
            .bind(("fid", source_ids[0].clone()))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<Value>>(0).ok())
            .unwrap_or_default();
        if still_exists.is_empty() { continue; }

        // Insert-before-delete: new fact is persisted before sources are removed.
        // If INSERT fails or embedder is down, sources are preserved (no data loss).
        let expires_at = now + 365 * 86400;
        let new_fid = uuid::Uuid::new_v4().to_string();
        let embedding_vec: Option<Vec<f32>> =
            crate::embedder::embed_text(client, embedding_url, &summary).await;
        let insert_ok = db
            .query("INSERT INTO memory_facts (fact_id, vault_id, account_id, content, category, expires_at, created_at, embedding) VALUES ($fid, $vid, $aid, $content, $cat, $exp, $now, $emb)")
            .bind(("fid", new_fid))
            .bind(("vid", vault_id.to_string()))
            .bind(("aid", account_id.to_string()))
            .bind(("content", summary))
            .bind(("cat", cat.to_string()))
            .bind(("exp", expires_at))
            .bind(("now", now))
            .bind(("emb", embedding_vec))
            .await
            .is_ok();

        if insert_ok {
            for fid in &source_ids {
                let _ = db.query("DELETE FROM memory_facts WHERE fact_id = $fid")
                    .bind(("fid", fid.clone())).await;
            }
            condensed += 1;
        }
    }

    Ok(json!({ "categories_condensed": condensed }))
}

// ── Tools schema ──────────────────────────────────────────────────────────────

fn build_tools_schema(tool_names: &[String]) -> Vec<Value> {
    let all = vec![
        json!({
            "type": "function",
            "function": {
                "name": "get_unprocessed_conversations",
                "description": "取得尚未分析記憶的對話列表（含標題與內容預覽）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": { "type": "number", "description": "最多取幾筆，預設 20" }
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "get_conversation_content",
                "description": "取得指定對話的訊息內容。使用 skip_count 跳過已處理過的舊訊息（從 get_unprocessed_conversations 取得 processed_msg_count 作為此值）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "conversation_id": { "type": "string", "description": "對話 ID" },
                        "skip_count": { "type": "number", "description": "跳過前 N 條訊息（已處理過的）。預設 0 表示讀取全部。" },
                        "char_limit": { "type": "number", "description": "每條訊息最多擷取幾個字元。預設 500，若 context 允許可設定至 1500 以讀取更完整內容。範圍 100-8000。" }
                    },
                    "required": ["conversation_id"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "save_memory_facts",
                "description": "將萃取的記憶事實儲存到記憶庫",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "conversation_id": { "type": "string", "description": "來源對話 ID" },
                        "facts": {
                            "type": "array",
                            "description": "事實列表，每條為 {content: string, category: string}",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "content": { "type": "string" },
                                    "category": { "type": "string", "description": "personal | preference | project | rule | general" }
                                },
                                "required": ["content"]
                            }
                        }
                    },
                    "required": ["conversation_id", "facts"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "mark_conversation_processed",
                "description": "標記對話已完成記憶分析（無論有無記憶價值都必須標記）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "conversation_id": { "type": "string" }
                    },
                    "required": ["conversation_id"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "condense_memory_facts",
                "description": "壓縮記憶事實：當某類別事實數量過多（≥8條）時，LLM 整合成 2-4 條精煉摘要。建議在 save_memory_facts 後呼叫。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "category": {
                            "type": "string",
                            "description": "要壓縮的類別（personal/preference/project/rule/general），省略則處理所有類別"
                        }
                    },
                    "required": []
                }
            }
        }),
    ];

    all.into_iter()
        .filter(|t| {
            let name = t["function"]["name"].as_str().unwrap_or("");
            tool_names.iter().any(|n| n == name)
        })
        .collect()
}

// ── DB helpers ────────────────────────────────────────────────────────────────

async fn load_agent_def(
    db: &SurrealDb,
    name: &str,
    account_id: &str,
) -> Option<Value> {
    let mut resp = db
        .query("SELECT * FROM agent_definitions WHERE name = $name AND account_id = $aid LIMIT 1")
        .bind(("name", name.to_string()))
        .bind(("aid", account_id.to_string()))
        .await
        .ok()?;

    let rows: Vec<Value> = resp.take(0).ok()?;
    rows.into_iter().next()
}

// ── Interactive agent (streaming, write-confirm, SSE) ─────────────────────────

/// Public entry point called from routes/agents.rs for user-facing agent runs.
/// Registers the session, runs a streaming agent loop, emits SSE events, then cleans up.
///
/// SSE events emitted:
///   llm:token           → plain string token
///   agent:tool_call     → {session_id, display}
///   agent:write_request → {session_id, display}  (awaits /agent/confirm before proceeding)
///   agent:note_refs     → {session_id, paths: []}
///   llm:done            → plain string (full response)
pub async fn run_interactive_agent(
    state: ApiState,
    session_id: String,
    messages: Vec<Value>,
    tool_names: Vec<String>,
    vault_id: String,
    account_id: String,
    vault_path: String,
) {
    // 1. Register cancel flag in session map
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut sessions = state.daemon.agent_sessions.lock().await;
        sessions.insert(session_id.clone(), AgentSession {
            cancel: Arc::clone(&cancel),
            confirm_tx: None,
        });
    }

    // 2. Resolve llm_url
    let llm_url = match state.daemon.llm_url.read().await.clone() {
        Some(u) => u,
        None => {
            state.daemon.emit("llm:done", json!(""));
            cleanup_session(&state, &session_id).await;
            return;
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .unwrap_or_default();
    let embedding_url = state.daemon.embedding_url.read().await.clone();
    let tools_schema = build_tools_schema_interactive(&tool_names);

    let mut msgs = messages;
    let mut full_response = String::new();

    'outer: for _round in 0..MAX_ROUNDS {
        if cancel.load(Ordering::Relaxed) { break; }

        let body = if tools_schema.is_empty() {
            json!({ "messages": msgs, "stream": true, "temperature": 0.7, "max_tokens": 2048 })
        } else {
            json!({
                "messages": msgs,
                "tools": tools_schema,
                "tool_choice": "auto",
                "stream": true,
                "temperature": 0.7,
                "max_tokens": 2048,
            })
        };

        // Stream one LLM round
        let (text, finish_reason, tool_chunks) = match stream_llm_round(
            &client, &llm_url, body, &state, &session_id, &cancel,
        ).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("[interactive] stream error: {}", e);
                break;
            }
        };

        if !text.is_empty() {
            full_response = text.clone();
        }

        if finish_reason == "tool_calls" && !tool_chunks.is_empty() {
            // Rebuild assistant tool_calls message
            let tc_json: Vec<Value> = tool_chunks.iter().map(|tc| json!({
                "id": tc.0, "type": "function",
                "function": { "name": tc.1, "arguments": tc.2 },
            })).collect();
            msgs.push(json!({ "role": "assistant", "content": null, "tool_calls": tc_json }));

            for (tc_id, tc_name, tc_args_str) in &tool_chunks {
                if cancel.load(Ordering::Relaxed) { break 'outer; }

                let args: Value = serde_json::from_str(tc_args_str).unwrap_or(json!({}));
                let display = format!("{}", tc_name);

                // Write tools require user confirmation
                if is_interactive_write_tool(tc_name) {
                    let (tx, rx) = oneshot::channel::<bool>();
                    {
                        let mut sessions = state.daemon.agent_sessions.lock().await;
                        if let Some(sess) = sessions.get_mut(session_id.as_str()) {
                            sess.confirm_tx = Some(tx);
                        }
                    }
                    state.daemon.emit("agent:write_request", json!({
                        "session_id": session_id,
                        "display": display,
                    }));

                    let approved = tokio::time::timeout(Duration::from_secs(120), rx)
                        .await
                        .unwrap_or(Ok(false))
                        .unwrap_or(false);

                    if !approved {
                        msgs.push(json!({
                            "role": "tool",
                            "tool_call_id": tc_id,
                            "content": "使用者拒絕了此操作",
                        }));
                        continue;
                    }
                } else {
                    state.daemon.emit("agent:tool_call", json!({
                        "session_id": session_id,
                        "display": display,
                    }));
                }

                let result = dispatch_interactive_tool(
                    &client, &llm_url, &state.db,
                    &vault_id, &account_id, &vault_path, &embedding_url,
                    tc_name, &args,
                ).await;

                let result_str = match result {
                    Ok(ref v) => {
                        // Emit note refs for read tools
                        let refs = extract_note_refs(tc_name, &args, v, &vault_path);
                        if !refs.is_empty() {
                            state.daemon.emit("agent:note_refs", json!({
                                "session_id": session_id,
                                "paths": refs,
                            }));
                        }
                        serde_json::to_string(v).unwrap_or_default()
                    }
                    Err(ref e) => format!("ERROR: {}", e),
                };

                msgs.push(json!({
                    "role": "tool",
                    "tool_call_id": tc_id,
                    "content": result_str,
                }));
            }
        } else {
            // No tool calls — done
            break;
        }
    }

    state.daemon.emit("llm:done", json!(full_response));
    cleanup_session(&state, &session_id).await;
}

async fn cleanup_session(state: &ApiState, session_id: &str) {
    let mut sessions = state.daemon.agent_sessions.lock().await;
    sessions.remove(session_id);
}

/// Stream one LLM round, emitting llm:token events. Returns (text, finish_reason, tool_chunks).
/// tool_chunks: Vec<(id, name, arguments_str)>
async fn stream_llm_round(
    client: &reqwest::Client,
    llm_url: &str,
    body: Value,
    state: &ApiState,
    _session_id: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<(String, String, Vec<(String, String, String)>), String> {
    let resp = client
        .post(format!("{}/v1/chat/completions", llm_url))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("llm error {}: {}", status, text));
    }

    let mut stream = resp.bytes_stream();
    let mut sse_buf = String::new();
    let mut full_text = String::new();
    let mut finish_reason = "stop".to_string();
    // tool_chunks: Vec<(id, name, arguments accumulated)>
    let mut tool_chunks: Vec<(String, String, String)> = Vec::new();

    while let Some(item) = stream.next().await {
        if cancel.load(Ordering::Relaxed) { break; }
        let bytes = item.map_err(|e| e.to_string())?;
        sse_buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(event_end) = sse_buf.find("\n\n") {
            let event = sse_buf[..event_end].to_string();
            sse_buf = sse_buf[event_end + 2..].to_string();

            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" { continue; }
                    if let Ok(j) = serde_json::from_str::<Value>(data) {
                        let choice = &j["choices"][0];
                        if let Some(fr) = choice["finish_reason"].as_str() {
                            if !fr.is_empty() { finish_reason = fr.to_string(); }
                        }
                        let delta = &choice["delta"];
                        if let Some(content) = delta["content"].as_str() {
                            if !content.is_empty() {
                                // Emit token as plain string value
                                state.daemon.emit("llm:token", json!(content));
                                full_text.push_str(content);
                            }
                        }
                        if let Some(tc_arr) = delta["tool_calls"].as_array() {
                            for tc in tc_arr {
                                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                                while tool_chunks.len() <= idx {
                                    tool_chunks.push((String::new(), String::new(), String::new()));
                                }
                                let acc = &mut tool_chunks[idx];
                                if let Some(id) = tc["id"].as_str() { if !id.is_empty() { acc.0 = id.to_string(); } }
                                if let Some(n) = tc["function"]["name"].as_str() { if !n.is_empty() { acc.1 = n.to_string(); } }
                                if let Some(a) = tc["function"]["arguments"].as_str() { acc.2.push_str(a); }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok((full_text, finish_reason, tool_chunks))
}

/// Write tool check for interactive agent (same as Tauri's is_write_tool)
fn is_interactive_write_tool(name: &str) -> bool {
    matches!(name, "create_note" | "update_note" | "create_folder")
}

/// Extract note paths for agent:note_refs event
fn extract_note_refs(tool_name: &str, args: &Value, _result: &Value, vault_path: &str) -> Vec<String> {
    let vp = std::path::Path::new(vault_path);
    match tool_name {
        "read_note" => {
            let p = args["path"].as_str().unwrap_or("");
            if p.is_empty() { return vec![]; }
            let full = if p.ends_with(".md") { p.to_string() } else { format!("{}.md", p) };
            vec![full]
        }
        "search_vault" => {
            // result is a string of lines; extract paths if possible
            // The result from dispatch may not have structured data here, so skip for now
            let _ = vp;
            vec![]
        }
        _ => vec![],
    }
}

/// Tool dispatcher for interactive agent (memory tools + vault filesystem tools)
async fn dispatch_interactive_tool(
    client: &reqwest::Client,
    llm_url: &str,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    vault_path: &str,
    embedding_url: &Option<String>,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    match name {
        // ── Vault read tools ─────────────────────────────────────────────────
        "list_structure" => {
            let path = args["path"].as_str().unwrap_or("");
            Ok(Value::String(vault_list_structure(path, vault_path)))
        }
        "read_note" => {
            let raw_path = args["path"].as_str().unwrap_or("");
            let path = if raw_path.ends_with(".md") { raw_path.to_string() } else { format!("{}.md", raw_path) };
            Ok(Value::String(vault_read_note(&path, vault_path)))
        }
        "search_vault" => {
            let query = args["query"].as_str().unwrap_or("");
            vault_search(db, vault_id, query).await
        }
        "query_memory" => {
            let keywords: Vec<String> = args["keywords"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
                .unwrap_or_default();
            let limit = args["limit"].as_u64().unwrap_or(5).min(20);
            vault_query_memory(db, vault_id, account_id, &keywords, limit).await
        }
        // ── Vault write tools ────────────────────────────────────────────────
        "create_note" => {
            let path = args["path"].as_str().unwrap_or("");
            let path = if path.ends_with(".md") { path.to_string() } else { format!("{}.md", path) };
            let content = args["content"].as_str().unwrap_or("");
            vault_create_note(&path, content, vault_path, client, db, vault_id).await
        }
        "update_note" => {
            let path = args["path"].as_str().unwrap_or("");
            let path = if path.ends_with(".md") { path.to_string() } else { format!("{}.md", path) };
            let content = args["content"].as_str().unwrap_or("");
            vault_update_note(&path, content, vault_path, client, db, vault_id).await
        }
        "create_folder" => {
            let path = args["path"].as_str().unwrap_or("");
            vault_create_folder(path, vault_path, db, vault_id).await
        }
        // ── Memory agent tools ───────────────────────────────────────────────
        "get_unprocessed_conversations" => {
            let limit = args["limit"].as_i64().unwrap_or(20);
            get_unprocessed_conversations(db, vault_id, account_id, limit).await
        }
        "get_conversation_content" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            let skip = args["skip_count"].as_i64().unwrap_or(0);
            let char_limit = args["char_limit"].as_i64().unwrap_or(500);
            get_conversation_content(db, &conv_id, skip, char_limit).await
        }
        "save_memory_facts" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            let facts = args["facts"].as_array().cloned().unwrap_or_default();
            save_memory_facts(client, db, vault_id, account_id, &conv_id, facts, embedding_url).await
        }
        "mark_conversation_processed" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            mark_conversation_processed(db, &conv_id).await
        }
        "condense_memory_facts" => {
            let category = args["category"].as_str().map(String::from);
            condense_memory_facts(client, llm_url, db, vault_id, account_id, category, embedding_url).await
        }
        _ => Err(format!("unknown tool: {}", name)),
    }
}

// ── Vault filesystem helpers ──────────────────────────────────────────────────

fn vault_list_structure(rel_path: &str, vault_path: &str) -> String {
    if vault_path.is_empty() { return "Vault 未設定".to_string(); }
    let base = std::path::Path::new(vault_path);
    let target = if rel_path.is_empty() { base.to_path_buf() } else { base.join(rel_path) };
    if !target.exists() { return format!("路徑不存在：{}", rel_path); }

    fn list_dir(dir: &std::path::Path, base: &std::path::Path, depth: u32) -> String {
        if depth > 4 { return String::new(); }
        let mut out = String::new();
        let Ok(entries) = std::fs::read_dir(dir) else { return out; };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().to_string();
            let indent = "  ".repeat(depth as usize);
            if path.is_dir() {
                out.push_str(&format!("{}{}/\n", indent, name));
                out.push_str(&list_dir(&path, base, depth + 1));
            } else if name.ends_with(".md") {
                out.push_str(&format!("{}{}\n", indent, rel));
            }
        }
        out
    }

    list_dir(&target, base, 0)
}

fn vault_read_note(rel_path: &str, vault_path: &str) -> String {
    if vault_path.is_empty() { return "Vault 未設定".to_string(); }
    if rel_path.is_empty() { return "路徑為空".to_string(); }
    let full = std::path::Path::new(vault_path).join(rel_path);
    match std::fs::read_to_string(&full) {
        Ok(c) => c,
        Err(_) => format!("讀取失敗：{}", rel_path),
    }
}

async fn vault_search(db: &SurrealDb, vault_id: &str, query: &str) -> Result<Value, String> {
    if query.is_empty() { return Ok(json!([])); }
    #[derive(serde::Deserialize)]
    struct Row { path: String, title: String }
    let mut resp = db
        .query("SELECT path, title FROM notes WHERE vault_id = $vid AND content ~ $q LIMIT 8")
        .bind(("vid", vault_id.to_string()))
        .bind(("q", query.to_string()))
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;
    let result: Vec<Value> = rows.iter().map(|r| json!({"path": r.path, "title": r.title})).collect();
    Ok(json!(result))
}

async fn vault_query_memory(
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    keywords: &[String],
    limit: u64,
) -> Result<Value, String> {
    let now = chrono::Utc::now().timestamp();
    #[derive(serde::Deserialize)]
    struct Row { content: String, category: String }
    let rows: Vec<Row> = if keywords.is_empty() {
        let mut r = db
            .query("SELECT content, category FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now ORDER BY created_at DESC LIMIT $lim")
            .bind(("vid", vault_id.to_string()))
            .bind(("aid", account_id.to_string()))
            .bind(("now", now))
            .bind(("lim", limit))
            .await
            .map_err(|e| e.to_string())?;
        r.take(0).map_err(|e| e.to_string())?
    } else {
        let mut collected: Vec<Row> = Vec::new();
        for kw in keywords.iter().take(3) {
            let mut r = db
                .query("SELECT content, category FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND content ~ $kw AND expires_at > $now LIMIT $lim")
                .bind(("vid", vault_id.to_string()))
                .bind(("aid", account_id.to_string()))
                .bind(("kw", kw.clone()))
                .bind(("now", now))
                .bind(("lim", limit))
                .await
                .map_err(|e| e.to_string())?;
            let rows: Vec<Row> = r.take(0).map_err(|e| e.to_string())?;
            collected.extend(rows);
        }
        collected
    };
    let result: Vec<Value> = rows.iter().map(|r| json!({"content": r.content, "category": r.category})).collect();
    Ok(json!(result))
}

async fn vault_create_note(
    rel_path: &str,
    content: &str,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    let full = std::path::Path::new(vault_path).join(rel_path);
    if let Some(parent) = full.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
    tokio::fs::write(&full, content).await.map_err(|e| e.to_string())?;
    // Sync note to DB
    sync_note_to_db(client, db, vault_id, rel_path, content).await;
    Ok(json!({ "ok": true, "path": rel_path }))
}

async fn vault_update_note(
    rel_path: &str,
    content: &str,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    let full = std::path::Path::new(vault_path).join(rel_path);
    if !full.exists() { return Err(format!("筆記不存在：{}", rel_path)); }
    tokio::fs::write(&full, content).await.map_err(|e| e.to_string())?;
    sync_note_to_db(client, db, vault_id, rel_path, content).await;
    Ok(json!({ "ok": true, "path": rel_path }))
}

async fn vault_create_folder(
    rel_path: &str,
    vault_path: &str,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    let full = std::path::Path::new(vault_path).join(rel_path);
    tokio::fs::create_dir_all(&full).await.map_err(|e| e.to_string())?;
    // Log folder creation in DB (best-effort)
    let now = chrono::Utc::now().timestamp();
    let _ = db
        .query("UPDATE vaults SET updated_at = $now WHERE vault_id = $vid")
        .bind(("now", now))
        .bind(("vid", vault_id.to_string()))
        .await;
    Ok(json!({ "ok": true, "path": rel_path }))
}

/// Best-effort: update the note record in DB so search index stays fresh.
async fn sync_note_to_db(
    _client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
    path: &str,
    content: &str,
) {
    let now = chrono::Utc::now().timestamp();
    let title = path.split('/').last().unwrap_or(path).trim_end_matches(".md").to_string();
    let note_id = uuid::Uuid::new_v4().to_string();
    let _ = db
        .query("INSERT INTO notes (note_id, vault_id, path, title, content, updated_at, created_at) VALUES ($nid, $vid, $path, $title, $content, $now, $now) ON DUPLICATE KEY UPDATE content = $content, title = $title, updated_at = $now")
        .bind(("nid", note_id))
        .bind(("vid", vault_id.to_string()))
        .bind(("path", path.to_string()))
        .bind(("title", title))
        .bind(("content", content.to_string()))
        .bind(("now", now))
        .await;
}

/// Build the tools schema for interactive agent (vault tools + memory tools).
/// Falls back to the scheduler's schema for memory-only tools.
fn build_tools_schema_interactive(tool_names: &[String]) -> Vec<Value> {
    // Vault tools (read)
    let vault_tools = vec![
        json!({ "type": "function", "function": {
            "name": "list_structure",
            "description": "列出 vault 的資料夾和筆記結構",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "子路徑，省略則顯示根目錄" }
            }, "required": [] }
        }}),
        json!({ "type": "function", "function": {
            "name": "read_note",
            "description": "讀取指定路徑的筆記內容",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "筆記的相對路徑（可省略 .md）" }
            }, "required": ["path"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "create_note",
            "description": "建立新筆記",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "筆記路徑（可省略 .md）" },
                "content": { "type": "string", "description": "筆記內容（Markdown）" }
            }, "required": ["path", "content"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "update_note",
            "description": "更新現有筆記的全部內容",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "筆記路徑（可省略 .md）" },
                "content": { "type": "string", "description": "新的筆記內容" }
            }, "required": ["path", "content"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "create_folder",
            "description": "建立新資料夾",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "資料夾相對路徑" }
            }, "required": ["path"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "search_vault",
            "description": "在 vault 中搜尋相關筆記",
            "parameters": { "type": "object", "properties": {
                "query": { "type": "string", "description": "搜尋關鍵字" }
            }, "required": ["query"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "query_memory",
            "description": "查詢長期記憶事實",
            "parameters": { "type": "object", "properties": {
                "keywords": { "type": "array", "items": { "type": "string" }, "description": "關鍵字列表" },
                "limit": { "type": "number", "description": "最多幾條，預設 5" }
            }, "required": [] }
        }}),
    ];

    // Combine vault tools + memory agent tools
    let all: Vec<Value> = vault_tools
        .into_iter()
        .chain(build_tools_schema(&tool_names.iter()
            .filter(|n| matches!(n.as_str(),
                "get_unprocessed_conversations" | "get_conversation_content" |
                "save_memory_facts" | "mark_conversation_processed" | "condense_memory_facts"
            ))
            .cloned()
            .collect::<Vec<_>>()))
        .collect();

    all.into_iter()
        .filter(|t| {
            let name = t["function"]["name"].as_str().unwrap_or("");
            tool_names.iter().any(|n| n == name)
        })
        .collect()
}
