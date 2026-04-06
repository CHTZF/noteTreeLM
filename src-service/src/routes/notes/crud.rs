use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app_state::ApiState;
use crate::routes::vault::get_vault_path;
use super::{PathQuery, extract_title_from_content, index_note_chunks};

#[derive(Deserialize)]
pub(super) struct ListNotesQuery {
    path_prefix: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct SetStatusBody {
    pub path: String,
    pub status: String,
}

pub(super) async fn list_notes(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<ListNotesQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    #[derive(serde::Deserialize)]
    struct NoteRow {
        path: String,
        title: String,
        word_count: i64,
        created_at: i64,
        modified_at: i64,
    }

    let rows: Vec<NoteRow> = if let Some(prefix) = q.path_prefix.filter(|s| !s.is_empty()) {
        let mut resp = state.db
            .query("SELECT path, title, word_count, created_at, modified_at FROM notes WHERE vault_id = $vid AND string::starts_with(path, $prefix) ORDER BY modified_at DESC")
            .bind(("vid", vault_id))
            .bind(("prefix", prefix))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).unwrap_or_default()
    } else {
        let mut resp = state.db
            .query("SELECT path, title, word_count, created_at, modified_at FROM notes WHERE vault_id = $vid ORDER BY modified_at DESC")
            .bind(("vid", vault_id))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).unwrap_or_default()
    };

    let out: Vec<Value> = rows.into_iter().map(|r| json!({
        "path": r.path, "title": r.title, "content": "",
        "word_count": r.word_count, "created_at": r.created_at, "modified_at": r.modified_at,
    })).collect();
    Ok(Json(json!(out)))
}

pub(super) async fn read_note(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = q.path.ok_or((StatusCode::BAD_REQUEST, "Missing path query param".to_string()))?;
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let full_path = std::path::Path::new(&vault_path).join(&rel_path);

    let content = std::fs::read_to_string(&full_path)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("Cannot read file: {}", e)))?;

    let title = extract_title_from_content(&content, &rel_path);
    let word_count = content.split_whitespace().count() as i64;
    let meta = std::fs::metadata(&full_path).ok();
    let to_ms = |t: std::time::SystemTime| {
        t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
    };
    let modified_at = meta.as_ref().and_then(|m| m.modified().ok()).map(to_ms).unwrap_or(0);
    let created_at  = meta.as_ref().and_then(|m| m.created().ok()).map(to_ms).unwrap_or(modified_at);
    let frontmatter = if content.starts_with("---") {
        content.strip_prefix("---")
            .and_then(|rest| rest.find("\n---").map(|end| rest[..end].trim().to_string()))
    } else { None };

    Ok(Json(json!({
        "path": rel_path, "title": title, "content": content,
        "frontmatter": frontmatter, "word_count": word_count,
        "created_at": created_at, "modified_at": modified_at,
    })))
}

pub(super) async fn create_note(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = if let Some(p) = body.get("path").and_then(|v| v.as_str()) {
        p.to_string()
    } else if let Some(title) = body.get("title").and_then(|v| v.as_str()) {
        let safe: String = title.chars()
            .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' { c } else { '_' })
            .collect();
        let filename = format!("{}.md", safe.trim());
        let folder = body.get("folder").and_then(|v| v.as_str()).unwrap_or("").trim_end_matches('/').to_string();
        if folder.is_empty() { filename } else { format!("{}/{}", folder, filename) }
    } else {
        return Err((StatusCode::BAD_REQUEST, "Missing path or title".to_string()));
    };
    let content = body.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let full_path = std::path::Path::new(&vault_path).join(&rel_path);
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    std::fs::write(&full_path, &content).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let title = extract_title_from_content(&content, &rel_path);
    let word_count = content.split_whitespace().count() as i64;
    let now = Utc::now().timestamp();
    state.db
        .query("INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at) VALUES ($vid, $path, $title, $content, $wc, $now, $now) ON DUPLICATE KEY UPDATE title = $title, content = $content, word_count = $wc, modified_at = $now")
        .bind(("vid", vault_id.clone())).bind(("path", rel_path.clone()))
        .bind(("title", title)).bind(("content", content.clone()))
        .bind(("wc", word_count)).bind(("now", now))
        .await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    index_note_chunks(&state, &vault_id, &rel_path, &content).await;
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn update_note(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = body.get("path").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?.to_string();
    let content  = body.get("content").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing content".to_string()))?.to_string();

    let vault_path = get_vault_path(&state, &vault_id).await?;
    std::fs::write(std::path::Path::new(&vault_path).join(&rel_path), &content)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let title = extract_title_from_content(&content, &rel_path);
    let word_count = content.split_whitespace().count() as i64;
    let now = Utc::now().timestamp();
    state.db
        .query("UPDATE notes SET title = $title, content = $content, word_count = $wc, modified_at = $now WHERE vault_id = $vid AND path = $path")
        .bind(("title", title)).bind(("content", content.clone()))
        .bind(("wc", word_count)).bind(("now", now))
        .bind(("vid", vault_id.clone())).bind(("path", rel_path.clone()))
        .await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    index_note_chunks(&state, &vault_id, &rel_path, &content).await;
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn delete_note(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = q.path.ok_or((StatusCode::BAD_REQUEST, "Missing path query param".to_string()))?;
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let full_path  = std::path::Path::new(&vault_path).join(&rel_path);

    if full_path.exists() {
        std::fs::remove_file(&full_path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    state.db
        .query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
        .bind(("vid", vault_id.clone())).bind(("path", rel_path.clone()))
        .await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = state.db
        .query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
        .bind(("vid", vault_id.clone())).bind(("fp", rel_path.clone())).await;
    {
        let sqlite = state.daemon.sqlite.clone();
        let vid = vault_id.clone(); let fp = rel_path.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(conn) = sqlite.lock() {
                if let Err(e) = crate::db::sqlite::fts_delete_file(&conn, &vid, &fp) {
                    tracing::warn!("SQLite FTS delete failed for {}: {}", fp, e);
                }
            }
        });
    }
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn set_note_status_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<SetStatusBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_path   = std::path::Path::new(&vault_path).join(&body.path);
    let content    = std::fs::read_to_string(&abs_path)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("Cannot read file: {}", e)))?;
    let updated = update_frontmatter_field(&content, "status", &body.status);
    std::fs::write(&abs_path, &updated).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let now = Utc::now().timestamp();
    let title = extract_title_from_content(&updated, &body.path);
    let word_count = updated.split_whitespace().count() as i64;
    let _ = state.db
        .query("UPDATE notes SET title = $title, content = $content, word_count = $wc, modified_at = $now WHERE vault_id = $vid AND path = $path")
        .bind(("title", title)).bind(("content", updated.clone()))
        .bind(("wc", word_count)).bind(("now", now))
        .bind(("vid", vault_id)).bind(("path", body.path))
        .await;
    Ok(Json(json!({ "ok": true })))
}

fn update_frontmatter_field(content: &str, key: &str, value: &str) -> String {
    if let Some(rest) = content.strip_prefix("---") {
        if let Some(fm_end) = rest.find("\n---") {
            let frontmatter = &rest[..fm_end];
            let after = &rest[fm_end + 4..];
            let key_prefix = format!("{}:", key);
            let updated_fm = if frontmatter.lines().any(|l| l.trim_start().starts_with(&key_prefix)) {
                frontmatter.lines()
                    .map(|l| if l.trim_start().starts_with(&key_prefix) { format!("{}: {}", key, value) } else { l.to_string() })
                    .collect::<Vec<_>>().join("\n")
            } else {
                format!("{}\n{}: {}", frontmatter.trim_end(), key, value)
            };
            return format!("---{}---{}", updated_fm, after);
        }
    }
    format!("---\n{}: {}\n---\n\n{}", key, value, content)
}
