use serde::{Deserialize, Serialize};
use sqlx::Row;
use tauri::State;
use uuid::Uuid;

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

// ── Commands ───────────────────────────────────────────────────────────────

/// 建立新對話，回傳 conversation_id（UUID）
#[tauri::command]
pub async fn create_conversation(
    state: State<'_, AppState>,
    username: String,
    mode: String,
    title: Option<String>,
) -> Result<String, AppError> {
    let db = &state.settings_db;
    let id = Uuid::new_v4().to_string();
    let title = title.unwrap_or_default();
    sqlx::query(
        "INSERT INTO conversations(id, account_id, mode, title) VALUES (?, ?, ?, ?)"
    )
    .bind(&id)
    .bind(&username)
    .bind(&mode)
    .bind(&title)
    .execute(db)
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
    let db = &state.settings_db;
    let limit = limit.unwrap_or(50);
    let offset = offset.unwrap_or(0);

    let rows = sqlx::query(
        "SELECT c.id, c.mode, c.title, c.updated_at,
                (SELECT 1 FROM pending_plans p WHERE p.conversation_id = c.id) AS has_plan
         FROM conversations c
         WHERE c.mode = ? AND c.account_id = ?
         ORDER BY c.updated_at DESC
         LIMIT ? OFFSET ?"
    )
    .bind(&mode)
    .bind(&username)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(rows.into_iter().map(|row| ConversationSummary {
        id: row.get("id"),
        mode: row.get("mode"),
        title: row.get("title"),
        updated_at: row.get("updated_at"),
        has_pending_plan: row.get::<Option<i64>, _>("has_plan").is_some(),
    }).collect())
}

/// 取得單一對話完整內容
#[tauri::command]
pub async fn get_conversation(
    state: State<'_, AppState>,
    id: String,
) -> Result<ConversationSnapshot, AppError> {
    let db = &state.settings_db;

    let row = sqlx::query(
        "SELECT c.id, c.mode, c.title, c.messages_json, c.created_at, c.updated_at,
                (SELECT 1 FROM pending_plans p WHERE p.conversation_id = c.id) AS has_plan
         FROM conversations c WHERE c.id = ?"
    )
    .bind(&id)
    .fetch_one(db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(ConversationSnapshot {
        id: row.get("id"),
        mode: row.get("mode"),
        title: row.get("title"),
        messages_json: row.get("messages_json"),
        has_pending_plan: row.get::<Option<i64>, _>("has_plan").is_some(),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

/// 刪除對話（pending_plan 由 ON DELETE CASCADE 自動清除）
#[tauri::command]
pub async fn delete_conversation(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), AppError> {
    let db = &state.settings_db;
    sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(&id)
        .execute(db)
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
    let db = &state.settings_db;
    sqlx::query(
        "UPDATE conversations SET title = ?, updated_at = strftime('%s','now') WHERE id = ?"
    )
    .bind(&title)
    .bind(&id)
    .execute(db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

// ── Internal helpers（供 invoke_agent 呼叫，不走 Tauri command）────────────

/// 讀取對話 messages_json（回傳 serde_json::Value）
/// 若 conversation_id 不存在（DB 被重置或 localStorage 殘留），回傳空陣列而非錯誤
pub async fn load_messages(db: &sqlx::SqlitePool, conv_id: &str) -> Result<serde_json::Value, AppError> {
    let row = sqlx::query("SELECT messages_json FROM conversations WHERE id = ?")
        .bind(conv_id)
        .fetch_optional(db)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let row = match row {
        None => return Ok(serde_json::json!([])),  // conversation 不存在，視為空對話
        Some(r) => r,
    };
    let json_str: String = row.get("messages_json");
    serde_json::from_str(&json_str).map_err(|e| AppError::AI(e.to_string()))
}

/// 儲存完整 messages_json 回 DB，同時更新 updated_at
/// 使用 UPSERT：若 conversation row 不存在（例如 localStorage 殘留 stale ID），自動建立
pub async fn save_messages(
    db: &sqlx::SqlitePool,
    conv_id: &str,
    messages: &serde_json::Value,
) -> Result<(), AppError> {
    let json_str = serde_json::to_string(messages).map_err(|e| AppError::AI(e.to_string()))?;
    sqlx::query(
        "INSERT INTO conversations(id, mode, messages_json, updated_at) \
         VALUES (?, 'chat', ?, strftime('%s','now')) \
         ON CONFLICT(id) DO UPDATE SET \
           messages_json = excluded.messages_json, \
           updated_at = excluded.updated_at"
    )
    .bind(conv_id)
    .bind(&json_str)
    .execute(db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// 如果對話標題還是空的，自動用第一條 user 訊息的前 20 字填入
pub async fn maybe_set_title(
    db: &sqlx::SqlitePool,
    conv_id: &str,
    messages: &serde_json::Value,
) -> Result<(), AppError> {
    // 只有 title 為空時才填
    let row = sqlx::query("SELECT title FROM conversations WHERE id = ?")
        .bind(conv_id)
        .fetch_optional(db)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let row = match row {
        None => return Ok(()), // conversation 不存在，跳過
        Some(r) => r,
    };
    let title: String = row.get("title");
    if !title.is_empty() {
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
                    sqlx::query(
                        "UPDATE conversations SET title = ? WHERE id = ? AND title = ''"
                    )
                    .bind(&auto_title)
                    .bind(conv_id)
                    .execute(db)
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

/// 儲存 pending plan（embedding centroids 以 LE f32 bytes 儲存）
pub async fn save_pending_plan(
    db: &sqlx::SqlitePool,
    conv_id: &str,
    deferred_tools: &[DeferredTool],
    confirm_centroid: &[f32],
    cancel_centroid: &[f32],
    interrupt_centroid: &[f32],
) -> Result<(), AppError> {
    let tools_json = serde_json::to_string(deferred_tools)
        .map_err(|e| AppError::AI(e.to_string()))?;

    let confirm_bytes = f32_slice_to_bytes(confirm_centroid);
    let cancel_bytes = f32_slice_to_bytes(cancel_centroid);
    let interrupt_bytes = f32_slice_to_bytes(interrupt_centroid);

    sqlx::query(
        "INSERT OR REPLACE INTO pending_plans
         (conversation_id, deferred_tools_json, confirm_centroid, cancel_centroid, interrupt_centroid)
         VALUES (?, ?, ?, ?, ?)"
    )
    .bind(conv_id)
    .bind(&tools_json)
    .bind(&confirm_bytes)
    .bind(&cancel_bytes)
    .bind(&interrupt_bytes)
    .execute(db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// 讀取 pending plan
pub async fn load_pending_plan(
    db: &sqlx::SqlitePool,
    conv_id: &str,
) -> Result<Option<LoadedPendingPlan>, AppError> {
    let row = sqlx::query(
        "SELECT deferred_tools_json, confirm_centroid, cancel_centroid, interrupt_centroid, created_at
         FROM pending_plans WHERE conversation_id = ?"
    )
    .bind(conv_id)
    .fetch_optional(db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    let row = match row {
        None => return Ok(None),
        Some(r) => r,
    };

    let tools_json: String = row.get("deferred_tools_json");
    let deferred_tools: Vec<DeferredTool> = serde_json::from_str(&tools_json)
        .map_err(|e| AppError::AI(e.to_string()))?;

    let confirm_centroid = bytes_to_f32_vec(row.get("confirm_centroid"));
    let cancel_centroid = bytes_to_f32_vec(row.get("cancel_centroid"));
    let interrupt_centroid = bytes_to_f32_vec(row.get("interrupt_centroid"));
    let created_at: i64 = row.get("created_at");

    Ok(Some(LoadedPendingPlan {
        deferred_tools,
        confirm_centroid,
        cancel_centroid,
        interrupt_centroid,
        created_at,
    }))
}

/// 刪除 pending plan
pub async fn delete_pending_plan(db: &sqlx::SqlitePool, conv_id: &str) -> Result<(), AppError> {
    sqlx::query("DELETE FROM pending_plans WHERE conversation_id = ?")
        .bind(conv_id)
        .execute(db)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// 清理超過 TTL（預設 24h）的過期 pending plans
pub async fn cleanup_expired_pending_plans(db: &sqlx::SqlitePool, ttl_secs: i64) -> Result<(), AppError> {
    let cutoff = chrono::Utc::now().timestamp() - ttl_secs;
    sqlx::query("DELETE FROM pending_plans WHERE created_at < ?")
        .bind(cutoff)
        .execute(db)
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

// ── Byte conversion helpers ───────────────────────────────────────────────

fn f32_slice_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for &f in v {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    bytes
}

fn bytes_to_f32_vec(bytes: Vec<u8>) -> Vec<f32> {
    bytes.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
