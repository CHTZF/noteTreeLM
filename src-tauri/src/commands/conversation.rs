use serde::{Deserialize, Serialize};
use tauri::State;
use uuid::Uuid;

use crate::db::surreal::SurrealDb;
use crate::error::AppError;
use crate::state::AppState;

// ── Response types ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ConversationSummary {
    pub id: String,
    pub mode: String,
    pub title: String,
    pub updated_at: i64,
    pub has_pending_plan: bool,
}

#[derive(Debug, Serialize)]
pub struct ConversationSnapshot {
    pub id: String,
    pub mode: String,
    pub title: String,
    pub messages_json: String,
    pub has_pending_plan: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

// ── Row deserialization helpers ─────────────────────────────────────────────

#[derive(Deserialize)]
struct ConvRow {
    id: String,
    mode: String,
    title: String,
    updated_at: surrealdb::sql::Datetime,
    created_at: surrealdb::sql::Datetime,
    messages_json: String,
}

// ── Commands ───────────────────────────────────────────────────────────────

/// 從前端儲存 messages_json 到 DB（供 ImportPanel / LiveChatPanel 等不走 invoke_agent 的 panel 使用）
#[tauri::command]
pub async fn save_conversation_messages(
    state: State<'_, AppState>,
    conversation_id: String,
    messages_json: String,
) -> Result<(), AppError> {
    let messages: serde_json::Value = serde_json::from_str(&messages_json)
        .map_err(|e| AppError::AI(e.to_string()))?;
    save_messages(&state.db, &conversation_id, &messages).await?;
    maybe_set_title(&state.db, &conversation_id, &messages).await?;
    Ok(())
}

/// 建立新對話，回傳 conversation_id（UUID）
#[tauri::command]
pub async fn create_conversation(
    state: State<'_, AppState>,
    username: String,
    mode: String,
    title: Option<String>,
) -> Result<String, AppError> {
    let db = &state.db;
    let id = Uuid::new_v4().to_string();
    let title = title.unwrap_or_default();
    db.query(
        "INSERT INTO conversations (id, account_id, mode, title) VALUES ($id, $account_id, $mode, $title)"
    )
    .bind(("id", id.clone()))
    .bind(("account_id", username.clone()))
    .bind(("mode", mode.clone()))
    .bind(("title", title.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(id)
}

/// 列出對話（分頁，按 updated_at DESC）
#[tauri::command]
pub async fn list_conversations(
    state: State<'_, AppState>,
    username: String,
    mode: String,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<ConversationSummary>, AppError> {
    let db = &state.db;
    let limit = limit.unwrap_or(50);
    let offset = offset.unwrap_or(0);

    // SurrealDB: LIMIT + START (offset)
    let mut resp = db.query(
        "SELECT record::id(id) AS id, mode, title, updated_at FROM conversations
         WHERE mode = $mode AND account_id = $account_id
         ORDER BY updated_at DESC
         LIMIT $limit START $offset"
    )
    .bind(("mode", mode.clone()))
    .bind(("account_id", username.clone()))
    .bind(("limit", limit))
    .bind(("offset", offset))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    #[derive(Deserialize)]
    struct ListRow {
        id: String,
        mode: String,
        title: String,
        updated_at: surrealdb::sql::Datetime,
    }
    let rows: Vec<ListRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    let mut summaries = Vec::new();
    for row in rows {
        // Check if pending plan exists
        let has_plan = has_pending_plan(db, &row.id).await?;
        summaries.push(ConversationSummary {
            id: row.id,
            mode: row.mode,
            title: row.title,
            updated_at: row.updated_at.timestamp(),
            has_pending_plan: has_plan,
        });
    }
    Ok(summaries)
}

/// 取得單一對話完整內容
#[tauri::command]
pub async fn get_conversation(
    state: State<'_, AppState>,
    id: String,
) -> Result<ConversationSnapshot, AppError> {
    let db = &state.db;

    let mut resp = db.query(
        "SELECT record::id(id) AS id, mode, title, messages_json, created_at, updated_at FROM type::thing('conversations', $id)"
    )
    .bind(("id", id.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    let rows: Vec<ConvRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    let row = rows.into_iter().next()
        .ok_or_else(|| AppError::Database(format!("conversation {} not found", id)))?;

    let has_plan = has_pending_plan(db, &row.id).await?;
    Ok(ConversationSnapshot {
        id: row.id,
        mode: row.mode,
        title: row.title,
        messages_json: row.messages_json,
        has_pending_plan: has_plan,
        created_at: row.created_at.timestamp(),
        updated_at: row.updated_at.timestamp(),
    })
}

/// 刪除對話（pending_plan 同步刪除）
#[tauri::command]
pub async fn delete_conversation(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), AppError> {
    let db = &state.db;
    db.query("DELETE type::thing('conversations', $id)")
        .bind(("id", id.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    db.query("DELETE FROM pending_plans WHERE conversation_id = $id")
        .bind(("id", id.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// 更新對話標題
#[tauri::command]
pub async fn update_conversation_title(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> Result<(), AppError> {
    let db = &state.db;
    db.query(
        "UPDATE type::thing('conversations', $id) SET title = $title, updated_at = time::now()"
    )
    .bind(("title", title.clone()))
    .bind(("id", id.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

// ── Internal helpers（供 invoke_agent 呼叫，不走 Tauri command）────────────

async fn has_pending_plan(db: &SurrealDb, conv_id: &str) -> Result<bool, AppError> {
    #[derive(Deserialize)]
    struct CountRow {
        count: i64,
    }
    let mut resp = db
        .query("SELECT count() AS count FROM pending_plans WHERE conversation_id = $id GROUP ALL")
        .bind(("id", conv_id.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<CountRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    Ok(rows.first().map(|r| r.count > 0).unwrap_or(false))
}

/// 讀取對話 messages_json（回傳 serde_json::Value）
/// 若 conversation_id 不存在（DB 被重置或 localStorage 殘留），回傳空陣列而非錯誤
pub async fn load_messages(db: &SurrealDb, conv_id: &str) -> Result<serde_json::Value, AppError> {
    #[derive(Deserialize)]
    struct MsgRow {
        messages_json: String,
    }
    let mut resp = db
        .query("SELECT messages_json FROM type::thing('conversations', $id)")
        .bind(("id", conv_id.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<MsgRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    match rows.into_iter().next() {
        None => Ok(serde_json::json!([])),  // conversation 不存在，視為空對話
        Some(r) => serde_json::from_str(&r.messages_json).map_err(|e| AppError::AI(e.to_string())),
    }
}

/// 儲存完整 messages_json 回 DB，同時更新 updated_at
/// 使用 UPSERT：若 conversation row 不存在（例如 localStorage 殘留 stale ID），自動建立
pub async fn save_messages(
    db: &SurrealDb,
    conv_id: &str,
    messages: &serde_json::Value,
) -> Result<(), AppError> {
    let json_str = serde_json::to_string(messages).map_err(|e| AppError::AI(e.to_string()))?;
    db.query(
        "UPDATE type::thing('conversations', $id) SET messages_json = $messages_json, updated_at = time::now()",
    )
    .bind(("id", conv_id.to_owned()))
    .bind(("messages_json", json_str.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// 如果對話標題還是空的，自動用第一條 user 訊息的前 20 字填入
pub async fn maybe_set_title(
    db: &SurrealDb,
    conv_id: &str,
    messages: &serde_json::Value,
) -> Result<(), AppError> {
    #[derive(Deserialize)]
    struct TitleRow {
        title: String,
    }
    let mut resp = db
        .query("SELECT title FROM type::thing('conversations', $id)")
        .bind(("id", conv_id.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<TitleRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    let row = match rows.into_iter().next() {
        None => return Ok(()), // conversation 不存在，跳過
        Some(r) => r,
    };
    if !row.title.is_empty() {
        return Ok(());
    }

    if let Some(arr) = messages.as_array() {
        for msg in arr {
            if msg["role"].as_str() == Some("user") {
                if let Some(content) = msg["content"].as_str() {
                    let chars: String = content.chars().take(20).collect();
                    let auto_title = if content.chars().count() > 20 {
                        format!("{}…", chars)
                    } else {
                        chars
                    };
                    db.query(
                        "UPDATE type::thing('conversations', $id) SET title = $title WHERE title = ''"
                    )
                    .bind(("title", auto_title.clone()))
                    .bind(("id", conv_id.to_owned()))
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                    break;
                }
            }
        }
    }
    Ok(())
}

// ── Pending plan helpers ───────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct DeferredTool {
    pub name: String,
    pub args: serde_json::Value,
}

/// 儲存 pending plan（embedding centroids 以 JSON array<float> 儲存）
pub async fn save_pending_plan(
    db: &SurrealDb,
    conv_id: &str,
    deferred_tools: &[DeferredTool],
    confirm_centroid: &[f32],
    cancel_centroid: &[f32],
    interrupt_centroid: &[f32],
) -> Result<(), AppError> {
    let tools_json = serde_json::to_string(deferred_tools)
        .map_err(|e| AppError::AI(e.to_string()))?;

    let confirm_vec: Vec<f64> = confirm_centroid.iter().map(|&f| f as f64).collect();
    let cancel_vec: Vec<f64> = cancel_centroid.iter().map(|&f| f as f64).collect();
    let interrupt_vec: Vec<f64> = interrupt_centroid.iter().map(|&f| f as f64).collect();

    db.query(
        "INSERT INTO pending_plans
         (conversation_id, deferred_tools_json, confirm_centroid, cancel_centroid, interrupt_centroid)
         VALUES ($conversation_id, $tools_json, $confirm, $cancel, $interrupt)
         ON DUPLICATE KEY UPDATE
           deferred_tools_json = $tools_json,
           confirm_centroid = $confirm,
           cancel_centroid = $cancel,
           interrupt_centroid = $interrupt",
    )
    .bind(("conversation_id", conv_id.to_owned()))
    .bind(("tools_json", tools_json.clone()))
    .bind(("confirm", confirm_vec))
    .bind(("cancel", cancel_vec))
    .bind(("interrupt", interrupt_vec))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// 讀取 pending plan
pub async fn load_pending_plan(
    db: &SurrealDb,
    conv_id: &str,
) -> Result<Option<LoadedPendingPlan>, AppError> {
    #[derive(Deserialize)]
    struct PlanRow {
        deferred_tools_json: String,
        confirm_centroid: Vec<f64>,
        cancel_centroid: Vec<f64>,
        interrupt_centroid: Vec<f64>,
        created_at: surrealdb::sql::Datetime,
    }
    let mut resp = db
        .query(
            "SELECT deferred_tools_json, confirm_centroid, cancel_centroid, interrupt_centroid, created_at
             FROM pending_plans WHERE conversation_id = $id LIMIT 1",
        )
        .bind(("id", conv_id.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<PlanRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    let row = match rows.into_iter().next() {
        None => return Ok(None),
        Some(r) => r,
    };

    let deferred_tools: Vec<DeferredTool> = serde_json::from_str(&row.deferred_tools_json)
        .map_err(|e| AppError::AI(e.to_string()))?;

    Ok(Some(LoadedPendingPlan {
        deferred_tools,
        confirm_centroid: row.confirm_centroid.iter().map(|&f| f as f32).collect(),
        cancel_centroid: row.cancel_centroid.iter().map(|&f| f as f32).collect(),
        interrupt_centroid: row.interrupt_centroid.iter().map(|&f| f as f32).collect(),
        created_at: row.created_at.timestamp(),
    }))
}

/// 刪除 pending plan
pub async fn delete_pending_plan(db: &SurrealDb, conv_id: &str) -> Result<(), AppError> {
    db.query("DELETE FROM pending_plans WHERE conversation_id = $id")
        .bind(("id", conv_id.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// 清理超過 TTL（預設 24h）的過期 pending plans
pub async fn cleanup_expired_pending_plans(db: &SurrealDb, ttl_secs: i64) -> Result<(), AppError> {
    let cutoff_ts = chrono::Utc::now().timestamp() - ttl_secs;
    // SurrealDB: compare datetime, cutoff as Unix epoch
    db.query(
        "DELETE FROM pending_plans WHERE time::unix(created_at) < $cutoff"
    )
    .bind(("cutoff", cutoff_ts))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

pub struct LoadedPendingPlan {
    pub deferred_tools: Vec<DeferredTool>,
    pub confirm_centroid: Vec<f32>,
    pub cancel_centroid: Vec<f32>,
    pub interrupt_centroid: Vec<f32>,
    pub created_at: i64,
}
