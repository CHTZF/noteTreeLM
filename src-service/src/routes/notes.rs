use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post, put},
    Json, Router,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api_state::ApiState;
use crate::routes::vault::get_vault_path;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/vaults/:vault_id/notes",
            get(list_notes).post(create_note).put(update_note).delete(delete_note),
        )
        .route("/vaults/:vault_id/notes/read", get(read_note))
}

#[derive(Deserialize)]
struct PathQuery {
    path: Option<String>,
}

async fn list_notes(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT vault_id, path, title, word_count, modified_at FROM notes WHERE vault_id = $vid ORDER BY modified_at DESC")
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn read_note(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = q
        .path
        .ok_or((StatusCode::BAD_REQUEST, "Missing path query param".to_string()))?;

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let full_path = std::path::Path::new(&vault_path).join(&rel_path);

    let content = std::fs::read_to_string(&full_path).map_err(|e| {
        (StatusCode::NOT_FOUND, format!("Cannot read file: {}", e))
    })?;

    Ok(Json(json!({ "content": content, "path": rel_path })))
}

async fn create_note(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = body
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?
        .to_string();
    let content = body
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let full_path = std::path::Path::new(&vault_path).join(&rel_path);

    // Create parent dirs
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    std::fs::write(&full_path, &content)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let title = extract_title_from_content(&content, &rel_path);
    let word_count = content.split_whitespace().count() as i64;
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at) VALUES ($vid, $path, $title, $content, $wc, $now, $now) ON DUPLICATE KEY UPDATE title = $title, content = $content, word_count = $wc, modified_at = $now")
        .bind(("vid", vault_id))
        .bind(("path", rel_path))
        .bind(("title", title))
        .bind(("content", content))
        .bind(("wc", word_count))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn update_note(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = body
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?
        .to_string();
    let content = body
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing content".to_string()))?
        .to_string();

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let full_path = std::path::Path::new(&vault_path).join(&rel_path);

    std::fs::write(&full_path, &content)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let title = extract_title_from_content(&content, &rel_path);
    let word_count = content.split_whitespace().count() as i64;
    let now = Utc::now().timestamp();

    state
        .db
        .query("UPDATE notes SET title = $title, content = $content, word_count = $wc, modified_at = $now WHERE vault_id = $vid AND path = $path")
        .bind(("title", title))
        .bind(("content", content))
        .bind(("wc", word_count))
        .bind(("now", now))
        .bind(("vid", vault_id))
        .bind(("path", rel_path))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn delete_note(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = q
        .path
        .ok_or((StatusCode::BAD_REQUEST, "Missing path query param".to_string()))?;

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let full_path = std::path::Path::new(&vault_path).join(&rel_path);

    if full_path.exists() {
        std::fs::remove_file(&full_path)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    state
        .db
        .query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
        .bind(("vid", vault_id))
        .bind(("path", rel_path))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

fn extract_title_from_content(content: &str, fallback_path: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("# ") {
            return heading.trim().to_string();
        }
        if !trimmed.is_empty() {
            break;
        }
    }
    std::path::Path::new(fallback_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| fallback_path.to_string())
}
