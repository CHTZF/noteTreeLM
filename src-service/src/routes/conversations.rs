use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, patch, post},
    Json, Router,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::app_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/conversations", get(list_conversations).post(create_conversation))
        .route(
            "/conversations/:id",
            get(get_conversation).delete(delete_conversation),
        )
        .route("/conversations/:id/title", patch(update_title))
        .route("/conversations/:id/messages", get(get_messages).put(save_messages))
        .route("/conversations/:id/processed", patch(mark_processed))
        .route(
            "/conversations/:id/pending-plan",
            get(get_pending_plan)
                .post(save_pending_plan_route)
                .delete(delete_pending_plan),
        )
        .route("/conversations/live-chat", post(get_or_create_live_chat))
        .route("/conversations/:id/ratings", get(get_conv_ratings))
        .route("/conversations/:id/rate", post(rate_conv_response))
        .route("/conversations/kb/:session_id/messages", get(get_kb_messages).put(save_kb_messages))
}

#[derive(Deserialize)]
struct ListQuery {
    vault_id: Option<String>,
    mode: Option<String>,
    account_id: Option<String>,
    limit: Option<u64>,
    offset: Option<u64>,
}

async fn list_conversations(
    State(state): State<ApiState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(50).min(200);
    let offset = q.offset.unwrap_or(0);
    let mut query_str = "SELECT *, record::id(id) AS id FROM conversations".to_string();
    let mut conditions = Vec::new();
    if q.vault_id.is_some() {
        conditions.push("vault_id = $vault_id");
    }
    if q.mode.is_some() {
        conditions.push("mode = $mode");
    }
    if q.account_id.is_some() {
        conditions.push("account_id = $account_id");
    }
    if !conditions.is_empty() {
        query_str.push_str(" WHERE ");
        query_str.push_str(&conditions.join(" AND "));
    }
    query_str.push_str(&format!(" ORDER BY updated_at DESC LIMIT {} START {}", limit, offset));

    let mut qb = state.db.query(&query_str);
    if let Some(vid) = q.vault_id {
        qb = qb.bind(("vault_id", vid));
    }
    if let Some(m) = q.mode {
        qb = qb.bind(("mode", m));
    }
    if let Some(aid) = q.account_id {
        qb = qb.bind(("account_id", aid));
    }

    let mut resp = qb
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Annotate each conversation with has_pending_plan
    let ids: Vec<String> = rows.iter()
        .filter_map(|r| r["id"].as_str().map(|s| s.to_string()))
        .collect();

    let plan_ids: std::collections::HashSet<String> = if ids.is_empty() {
        Default::default()
    } else {
        match state.db
            .query("SELECT conversation_id FROM pending_plans WHERE conversation_id IN $ids")
            .bind(("ids", ids))
            .await
        {
            Ok(mut pr) => {
                let plan_rows: Vec<Value> = pr.take(0).unwrap_or_default();
                plan_rows.iter()
                    .filter_map(|r| r["conversation_id"].as_str().map(|s| s.to_string()))
                    .collect()
            }
            Err(_) => Default::default(),
        }
    };

    let annotated: Vec<Value> = rows.into_iter().map(|mut r| {
        let id = r["id"].as_str().unwrap_or("").to_string();
        if let Some(obj) = r.as_object_mut() {
            obj.insert("has_pending_plan".to_string(), json!(plan_ids.contains(&id)));
        }
        r
    }).collect();

    Ok(Json(json!(annotated)))
}

async fn create_conversation(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let account_id = body
        .get("account_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let vault_id = body
        .get("vault_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mode = body
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("chat")
        .to_string();
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("New Conversation")
        .to_string();
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO conversations (id, account_id, vault_id, mode, title, messages_json, created_at, updated_at) VALUES ($id, $account_id, $vault_id, $mode, $title, '[]', $now, $now)")
        .bind(("id", id.clone()))
        .bind(("account_id", account_id))
        .bind(("vault_id", vault_id))
        .bind(("mode", mode))
        .bind(("title", title))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "id": id })))
}

async fn get_conversation(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT *, record::id(id) AS id FROM conversations WHERE id = type::thing(\"conversations\", $id) LIMIT 1")
        .bind(("id", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match rows.into_iter().next() {
        Some(mut row) => {
            // Check if there's a pending plan for this conversation
            let conv_id = row["id"].as_str().unwrap_or("").to_string();
            let has_plan = if !conv_id.is_empty() {
                let mut pr = state.db
                    .query("SELECT count() FROM pending_plans WHERE conversation_id = $id LIMIT 1")
                    .bind(("id", conv_id))
                    .await
                    .ok();
                if let Some(ref mut r) = pr {
                    let plan_rows: Vec<Value> = r.take(0).unwrap_or_default();
                    plan_rows.first()
                        .and_then(|v| v["count"].as_u64())
                        .unwrap_or(0) > 0
                } else { false }
            } else { false };
            if let Some(obj) = row.as_object_mut() {
                obj.insert("has_pending_plan".to_string(), json!(has_plan));
            }
            Ok(Json(row))
        }
        None => Err((StatusCode::NOT_FOUND, "Conversation not found".to_string())),
    }
}

async fn delete_conversation(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state
        .db
        .query("DELETE FROM conversations WHERE id = type::thing(\"conversations\", $id)")
        .bind(("id", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn update_title(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing title".to_string()))?
        .to_string();
    let only_if_empty = body.get("only_if_empty").and_then(|v| v.as_bool()).unwrap_or(false);
    let now = Utc::now().timestamp();

    if only_if_empty {
        state
            .db
            .query("UPDATE conversations SET title = $title, updated_at = $now WHERE id = type::thing(\"conversations\", $id) AND (title = '' OR title IS NONE)")
            .bind(("title", title))
            .bind(("now", now))
            .bind(("id", id))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    } else {
        state
            .db
            .query("UPDATE conversations SET title = $title, updated_at = $now WHERE id = type::thing(\"conversations\", $id)")
            .bind(("title", title))
            .bind(("now", now))
            .bind(("id", id))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    Ok(Json(json!({ "ok": true })))
}

async fn get_messages(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT messages_json FROM conversations WHERE id = type::thing(\"conversations\", $id) LIMIT 1")
        .bind(("id", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    #[derive(serde::Deserialize)]
    struct MsgRow { messages_json: String }
    let rows: Vec<MsgRow> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match rows.into_iter().next() {
        Some(row) => Ok(Json(json!({ "messages_json": row.messages_json }))),
        None => Err((StatusCode::NOT_FOUND, "Conversation not found".to_string())),
    }
}

async fn save_messages(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Accept either "messages_json" (string) or "messages" (array)
    let messages_json = if let Some(mj) = body.get("messages_json").and_then(|v| v.as_str()) {
        mj.to_string()
    } else if let Some(messages) = body.get("messages") {
        serde_json::to_string(messages)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        return Err((StatusCode::BAD_REQUEST, "Missing messages or messages_json".to_string()));
    };
    let now = Utc::now().timestamp();

    state
        .db
        .query("UPDATE conversations SET messages_json = $mj, updated_at = $now WHERE id = type::thing(\"conversations\", $id)")
        .bind(("mj", messages_json.clone()))
        .bind(("now", now))
        .bind(("id", id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Trigger memory agent check in background (non-blocking)
    tokio::spawn(maybe_trigger_memory_agent(state, id, messages_json));

    Ok(Json(json!({ "ok": true })))
}

async fn maybe_trigger_memory_agent(state: ApiState, conv_id: String, messages_json: String) {
    // Parse messages, filter to user+assistant only
    let msgs: Vec<Value> = serde_json::from_str::<Vec<Value>>(&messages_json)
        .ok()
        .map(|arr| arr.into_iter().filter(|m| {
            matches!(m["role"].as_str(), Some("user") | Some("assistant"))
        }).collect())
        .unwrap_or_default();

    if msgs.is_empty() { return }

    // Fetch conversation metadata (account_id, vault_id)
    #[derive(serde::Deserialize)]
    struct ConvMeta { account_id: Option<String>, vault_id: Option<String> }
    let Ok(mut resp) = state.db
        .query("SELECT account_id, vault_id FROM conversations WHERE id = type::thing(\"conversations\", $id) LIMIT 1")
        .bind(("id", conv_id.clone()))
        .await
    else { return };

    let rows: Vec<ConvMeta> = resp.take(0).unwrap_or_default();
    let Some(meta) = rows.into_iter().next() else { return };
    let account_id = meta.account_id.unwrap_or_default();
    let vault_id = meta.vault_id.unwrap_or_default();
    if account_id.is_empty() || vault_id.is_empty() { return }

    // Read user memory settings
    let (enabled, threshold) = get_user_memory_settings(&state.db, &account_id).await;
    if !enabled { return }
    if threshold == 0 { return }

    // Ensure a periodic memory_agent schedule exists for this vault+account.
    // Handles the case where the vault was created before auto_memory was enabled.
    crate::db::seeds::ensure_memory_schedule(&state.db, &vault_id, &account_id).await;

    // Trigger only at exact multiples of threshold
    if msgs.len() % threshold as usize != 0 { return }

    // LLM judgment: short 5s timeout + fail-closed.
    // If LLM is busy serving chat, skip silently; the 8h scheduler picks it up later.
    if !llm_should_remember(&state, &msgs).await {
        tracing::debug!("[memory] conv {} skipped: LLM judged no memory value", conv_id);
        return;
    }

    // Fetch the watermark so the agent can skip already-processed messages
    #[derive(serde::Deserialize)]
    struct WatermarkRow { memory_processed_msg_count: Option<i64> }
    let skip_count = state.db
        .query("SELECT memory_processed_msg_count FROM conversations WHERE id = type::thing(\"conversations\", $id) LIMIT 1")
        .bind(("id", conv_id.clone()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<WatermarkRow>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .and_then(|r| r.memory_processed_msg_count)
        .unwrap_or(0);

    let task_id     = uuid::Uuid::new_v4().to_string();
    let initial_msg = format!("conv_id:{} skip_count:{}", conv_id, skip_count);
    let description = "auto memory".to_string();
    tokio::spawn(async move {
        let agent_def = match crate::service::helpers::load_agent_def(
            &state.db, "memory_agent", &account_id,
        ).await {
            Some(a) => a,
            None => {
                tracing::warn!("[memory] memory_agent def not found for account '{}'", account_id);
                return;
            }
        };
        let conv_id = format!("scheduled_{}_memory_agent_{}", task_id, chrono::Utc::now().timestamp());
        let runtime = match crate::service::build_agent_runtime(
            &state, &vault_id, &account_id,
            None, conv_id, agent_def,
            false,
            None, // no ui_language for background tasks
            None, None,
        ).await {
            Some(r) => r,
            None => return,
        };
        crate::service::execute_scheduled_task(runtime, task_id, description, initial_msg).await;
    });
}

/// LLM-based judgment for ambiguous cases not caught by keyword heuristic.
/// Uses a 5s timeout and is fail-closed: if the LLM server is busy (serving chat),
/// returns false and lets the 8h scheduler handle the conversation later.
async fn llm_should_remember(state: &ApiState, msgs: &[Value]) -> bool {
    let llm_url = state.daemon.llm_url.clone();

    let excerpt = msgs.iter().rev().take(6).collect::<Vec<_>>().into_iter().rev()
        .map(|m| {
            let role = m["role"].as_str().unwrap_or("user");
            let content: String = m["content"].as_str().unwrap_or("").chars().take(120).collect();
            format!("[{}]: {}", role, content)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let body = serde_json::json!({
        "messages": [
            {
                "role": "system",
                "content": "你是記憶價值評估器。判斷以下對話片段是否包含值得記憶的資訊。以下任一情況回答 yes：使用者提到居住地點或所在城市、慣用語言、職業身份、交通習慣；或提到偏好、重要決策、習慣規則、專案背景。純粹問答、搜尋結果、閒聊回答 no。只回答 yes 或 no，不要解釋。"
            },
            { "role": "user", "content": excerpt }
        ],
        "stream": false,
        "temperature": 0.0,
        "max_tokens": 5,
    });

    // Short timeout: if LLM is busy serving chat requests, give up rather than queue behind them.
    // fail-closed → return false; the 8h scheduler will catch this conversation later.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();

    let Ok(resp) = client
        .post(format!("{}/v1/chat/completions", llm_url))
        .json(&body)
        .send()
        .await
    else { return false }; // fail-closed: timeout or connection error → skip

    let Ok(json) = resp.json::<Value>().await else { return false };

    let answer = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_lowercase();

    answer.contains("yes")
}


async fn get_user_memory_settings(db: &crate::db::SurrealDb, account_id: &str) -> (bool, u32) {
    #[derive(serde::Deserialize)]
    struct Row { key: String, value: String }
    let Ok(mut resp) = db
        .query("SELECT `key`, `value` FROM user_settings WHERE username = $u AND `key` IN ['enable_auto_memory', 'memory_threshold']")
        .bind(("u", account_id.to_string()))
        .await
    else { return (false, 20) };
    let rows: Vec<Row> = resp.take(0).unwrap_or_default();
    let mut enabled = false;
    let mut threshold = 20u32;
    for r in rows {
        match r.key.as_str() {
            "enable_auto_memory" => enabled = r.value == "true",
            "memory_threshold"   => threshold = r.value.parse().unwrap_or(20),
            _ => {}
        }
    }
    (enabled, threshold)
}

async fn mark_processed(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    state
        .db
        .query("UPDATE conversations SET memory_processed_at = $now, updated_at = $now WHERE id = type::thing(\"conversations\", $id)")
        .bind(("now", now))
        .bind(("id", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn get_pending_plan(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT deferred_tools_json, created_at FROM pending_plans WHERE conversation_id = $id LIMIT 1")
        .bind(("id", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    #[derive(serde::Deserialize)]
    struct PlanRow {
        deferred_tools_json: String,
        created_at: i64,
    }
    let rows: Vec<PlanRow> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match rows.into_iter().next() {
        Some(row) => {
            let tools: Value = serde_json::from_str(&row.deferred_tools_json)
                .unwrap_or(json!([]));
            Ok(Json(json!({ "deferred_tools": tools, "created_at": row.created_at })))
        },
        None => Err((StatusCode::NOT_FOUND, "No pending plan".to_string())),
    }
}

async fn save_pending_plan_route(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Accept "deferred_tools" (array) or "deferred_tools_json" (string)
    let tools_json = if let Some(arr) = body.get("deferred_tools") {
        serde_json::to_string(arr)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        body.get("deferred_tools_json")
            .and_then(|v| v.as_str())
            .unwrap_or("[]")
            .to_string()
    };
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO pending_plans (conversation_id, deferred_tools_json, created_at) VALUES ($id, $tools, $now) ON DUPLICATE KEY UPDATE deferred_tools_json = $tools")
        .bind(("id", id))
        .bind(("tools", tools_json))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn delete_pending_plan(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state
        .db
        .query("DELETE FROM pending_plans WHERE conversation_id = $id")
        .bind(("id", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn get_or_create_live_chat(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let account_id = body
        .get("account_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let vault_id = body
        .get("vault_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    #[derive(serde::Deserialize)]
    struct IdRow {
        id: String,
    }

    let mut resp = state
        .db
        .query("SELECT record::id(id) AS id FROM conversations WHERE mode = 'live_chat' AND vault_id = $vault_id AND account_id = $account_id LIMIT 1")
        .bind(("account_id", account_id.clone()))
        .bind(("vault_id", vault_id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<IdRow> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if let Some(row) = rows.into_iter().next() {
        return Ok(Json(json!({ "id": row.id })));
    }

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    state
        .db
        .query("INSERT INTO conversations (id, account_id, vault_id, mode, title, messages_json, created_at, updated_at) VALUES ($id, $account_id, $vault_id, 'live_chat', 'Live Chat', '[]', $now, $now)")
        .bind(("id", id.clone()))
        .bind(("account_id", account_id))
        .bind(("vault_id", vault_id))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "id": id })))
}

// ── Conversation Ratings ───────────────────────────────────────────────────────

async fn get_conv_ratings(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT content_hash, rating, created_at FROM response_ratings WHERE conversation_id = $cid ORDER BY created_at DESC")
        .bind(("cid", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Value> = resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!(rows)))
}

async fn rate_conv_response(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let content_hash = body.get("content_hash").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing content_hash".to_string()))?.to_string();
    let rating = body.get("rating").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing rating".to_string()))?.to_string();
    let rating_id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();

    // Lookup vault_id from the conversation so ratings are properly vault-scoped
    let vault_id = state.db
        .query("SELECT vault_id FROM conversations WHERE id = type::thing(\"conversations\", $id) LIMIT 1")
        .bind(("id", id.clone()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<Value>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .and_then(|v| v["vault_id"].as_str().map(|s| s.to_string()))
        .unwrap_or_default();

    state.db
        .query("INSERT INTO response_ratings (id, rating_id, vault_id, conversation_id, content_hash, rating, created_at) VALUES ($rid, $rid, $vid, $cid, $hash, $rat, $now)")
        .bind(("rid", rating_id))
        .bind(("vid", vault_id))
        .bind(("cid", id))
        .bind(("hash", content_hash))
        .bind(("rat", rating))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

// ── KB Session Chat Messages ───────────────────────────────────────────────────

async fn get_kb_messages(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT messages_json FROM conversations WHERE id = type::thing(\"conversations\", $id) AND mode = 'kb_session' LIMIT 1")
        .bind(("id", format!("kb_{}", session_id)))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Value> = resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let messages = rows.first().and_then(|r| r["messages_json"].as_str()).map(|s| s.to_string());
    Ok(Json(json!(messages)))
}

async fn save_kb_messages(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let messages = body.get("messages").and_then(|v| v.as_str()).unwrap_or("[]").to_string();
    let id = format!("kb_{}", session_id);
    let now = Utc::now().timestamp();

    state.db
        .query("INSERT INTO conversations (id, mode, messages_json, created_at, updated_at) VALUES ($id, 'kb_session', $msgs, $now, $now) ON DUPLICATE KEY UPDATE messages_json = $msgs, updated_at = $now")
        .bind(("id", id))
        .bind(("msgs", messages))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}
