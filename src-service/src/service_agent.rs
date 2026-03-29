/// service_agent.rs
///
/// Service-side sub-agent runner for scheduled tasks.
///
/// Flow:
///   1. Look up agent_definitions by name + account_id
///   2. Build tool registry (service-side tools only)
///   3. Run agentic LLM loop (max_rounds)
///   4. Emit SSE when done

use serde_json::{json, Value};
use crate::api_state::ApiState;
use crate::db::SurrealDb;
use chrono::Local;

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

    let result = run_agent_loop(
        &client,
        &llm_url,
        &state.db,
        &vault_id,
        &account_id,
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

                let result = dispatch_tool(client, llm_url, db, vault_id, account_id, &fn_name, &fn_args).await;
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
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    match name {
        "get_unprocessed_conversations" => {
            let limit = args["limit"].as_i64().unwrap_or(20);
            get_unprocessed_conversations(db, vault_id, limit).await
        }
        "get_conversation_content" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            get_conversation_content(db, &conv_id).await
        }
        "save_memory_facts" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            let facts = args["facts"].as_array().cloned().unwrap_or_default();
            save_memory_facts(db, vault_id, account_id, &conv_id, facts).await
        }
        "mark_conversation_processed" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            mark_conversation_processed(db, &conv_id).await
        }
        "condense_memory_facts" => {
            let category = args["category"].as_str().map(String::from);
            condense_memory_facts(client, llm_url, db, vault_id, category).await
        }
        "distill_preferences" => {
            distill_preferences(client, llm_url, db, vault_id, account_id).await
        }
        _ => Err(format!("unknown tool: {}", name)),
    }
}

// ── Tool implementations ──────────────────────────────────────────────────────

async fn get_unprocessed_conversations(
    db: &SurrealDb,
    vault_id: &str,
    limit: i64,
) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: String,
        title: String,
        updated_at: i64,
        messages_json: Option<String>,
    }

    let mut resp = db
        .query("SELECT record::id(id) AS id, title, updated_at, messages_json \
                FROM conversations \
                WHERE vault_id = $vid AND (memory_processed = NONE OR memory_processed = false) \
                ORDER BY updated_at DESC LIMIT $lim")
        .bind(("vid", vault_id.to_string()))
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
            "preview": preview,
        })
    }).collect();

    Ok(json!(items))
}

async fn get_conversation_content(
    db: &SurrealDb,
    conv_id: &str,
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

    let text = arr.iter()
        .filter(|m| matches!(m["role"].as_str(), Some("user") | Some("assistant")))
        .map(|m| {
            let role = m["role"].as_str().unwrap_or("user");
            let content: String = m["content"].as_str().unwrap_or("").chars().take(500).collect();
            format!("[{}]: {}", role, content)
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    Ok(Value::String(text))
}

async fn save_memory_facts(
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    conv_id: &str,
    facts: Vec<Value>,
) -> Result<Value, String> {
    let valid_cats = ["personal", "preference", "project", "rule", "general"];
    let now = chrono::Utc::now().timestamp();
    let expires_at = now + 60 * 60 * 24 * 365;
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

        let fact_id = uuid::Uuid::new_v4().to_string();
        let _ = db
            .query("INSERT INTO memory_facts (fact_id, vault_id, account_id, conv_id, content, category, expires_at, created_at) \
                    VALUES ($fid, $vid, $aid, $cid, $content, $cat, $exp, $now)")
            .bind(("fid", fact_id))
            .bind(("vid", vault_id.to_string()))
            .bind(("aid", account_id.to_string()))
            .bind(("cid", conv_id.to_string()))
            .bind(("content", content))
            .bind(("cat", category))
            .bind(("exp", expires_at))
            .bind(("now", now))
            .await;
        count += 1;
    }

    Ok(json!({ "facts_saved": count }))
}

async fn mark_conversation_processed(
    db: &SurrealDb,
    conv_id: &str,
) -> Result<Value, String> {
    db.query("UPDATE conversations SET memory_processed = true WHERE record::id(id) = $cid")
        .bind(("cid", conv_id.to_string()))
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

async fn condense_memory_facts(
    client: &reqwest::Client,
    llm_url: &str,
    db: &SurrealDb,
    vault_id: &str,
    category: Option<String>,
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
            .query("SELECT fact_id, content FROM memory_facts WHERE vault_id = $vid AND category = $cat AND expires_at > $now ORDER BY created_at DESC LIMIT 50")
            .bind(("vid", vault_id.to_string()))
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

        for fid in &source_ids {
            let _ = db.query("DELETE FROM memory_facts WHERE fact_id = $fid")
                .bind(("fid", fid.clone())).await;
        }
        let expires_at = now + 60 * 60 * 24 * 365;
        let new_fid = uuid::Uuid::new_v4().to_string();
        let _ = db.query("INSERT INTO memory_facts (fact_id, vault_id, content, category, expires_at, created_at) VALUES ($fid, $vid, $content, $cat, $exp, $now)")
            .bind(("fid", new_fid))
            .bind(("vid", vault_id.to_string()))
            .bind(("content", summary))
            .bind(("cat", cat.to_string()))
            .bind(("exp", expires_at))
            .bind(("now", now))
            .await;
        condensed += 1;
    }

    Ok(json!({ "categories_condensed": condensed }))
}

async fn distill_preferences(
    client: &reqwest::Client,
    llm_url: &str,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
) -> Result<Value, String> {
    let now = chrono::Utc::now().timestamp();

    #[derive(serde::Deserialize)]
    struct FactRow { content: String, category: String }

    let Ok(mut resp) = db
        .query("SELECT content, category FROM memory_facts WHERE vault_id = $vid AND category IN ['personal','preference','rule'] AND expires_at > $now ORDER BY created_at DESC LIMIT 30")
        .bind(("vid", vault_id.to_string()))
        .bind(("now", now))
        .await
    else { return Err("DB query failed".to_string()) };

    let facts: Vec<FactRow> = resp.take(0).unwrap_or_default();
    if facts.is_empty() { return Ok(json!({ "ok": true, "skipped": "no facts" })) }

    let combined = facts.iter()
        .map(|f| format!("[{}] {}", f.category, f.content))
        .collect::<Vec<_>>().join("\n");

    let body = json!({
        "messages": [
            { "role": "system", "content": "你是使用者偏好分析系統。從以下記憶事實中，整理出使用者的偏好規則。\n\
                輸出格式：條列式，每條以「- 」開頭，簡潔描述。不超過 15 條。只輸出列表，不要說明。" },
            { "role": "user", "content": format!("記憶事實：\n{}", combined) }
        ],
        "stream": false, "temperature": 0.3, "max_tokens": 512,
    });

    let Ok(resp) = client.post(format!("{}/v1/chat/completions", llm_url))
        .json(&body).send().await
    else { return Err("LLM request failed".to_string()) };
    let Ok(resp_json) = resp.json::<Value>().await
    else { return Err("LLM response parse failed".to_string()) };

    let prefs = match resp_json["choices"][0]["message"]["content"].as_str() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return Ok(json!({ "ok": true, "skipped": "empty LLM response" })),
    };

    let updated_time = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    let behavior = format!("更新時間：{}\n\n{}", updated_time, prefs);
    let skill_id = "distilled_user_prefs".to_string();

    let existing: Vec<Value> = db
        .query("SELECT record::id(id) AS id FROM agent_skills WHERE account_id = $aid AND skill_id = $sid LIMIT 1")
        .bind(("aid", account_id.to_string()))
        .bind(("sid", skill_id.clone()))
        .await.ok()
        .and_then(|mut r| r.take::<Vec<Value>>(0).ok())
        .unwrap_or_default();

    if existing.is_empty() {
        let _ = db.query("INSERT INTO agent_skills (account_id, skill_id, title, trigger, behavior, is_active, injection_mode, created_at, updated_at) VALUES ($aid, $sid, $title, $trigger, $behavior, true, 'system', $now, $now)")
            .bind(("aid", account_id.to_string()))
            .bind(("sid", skill_id.clone()))
            .bind(("title", "使用者偏好（自動蒸餾）".to_string()))
            .bind(("trigger", "__auto_injected__".to_string()))
            .bind(("behavior", behavior.clone()))
            .bind(("now", now))
            .await;
    } else {
        let _ = db.query("UPDATE agent_skills SET behavior = $behavior, updated_at = $now WHERE account_id = $aid AND skill_id = $sid")
            .bind(("behavior", behavior.clone()))
            .bind(("now", now))
            .bind(("aid", account_id.to_string()))
            .bind(("sid", skill_id))
            .await;
    }

    Ok(json!({ "ok": true }))
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
                "description": "取得指定對話的完整訊息內容",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "conversation_id": { "type": "string", "description": "對話 ID" }
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
        json!({
            "type": "function",
            "function": {
                "name": "distill_preferences",
                "description": "將 personal/preference/rule 類別的記憶事實蒸餾為使用者偏好規則，更新到 agent_skill（distilled_user_prefs）。建議在 condense_memory_facts 後呼叫。",
                "parameters": {
                    "type": "object",
                    "properties": {},
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
