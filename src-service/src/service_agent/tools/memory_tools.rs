use serde_json::{json, Value};
use crate::db::SurrealDb;

pub(crate) async fn get_unprocessed_conversations(
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

pub(crate) async fn get_conversation_content(
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

pub(crate) async fn save_memory_facts(
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
            crate::processing::embedder::embed_text(client, embedding_url, &content).await;

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

pub(crate) async fn mark_conversation_processed(
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

pub(crate) async fn condense_memory_facts(
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
            crate::processing::embedder::embed_text(client, embedding_url, &summary).await;
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

/// Build the tools schema for scheduled agent (memory tools only).
pub(crate) fn build_scheduled_tools_schema(tool_names: &[String]) -> Vec<Value> {
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

/// Tool dispatcher for scheduled agent (memory tools only).
pub(crate) async fn dispatch_scheduled_tool(
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
