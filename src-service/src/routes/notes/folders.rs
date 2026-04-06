use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use serde_json::{json, Value};

use crate::app_state::ApiState;
use crate::routes::vault::get_vault_path;
use super::{extract_title_from_content, index_note_chunks};

pub(super) async fn create_folder_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let folder_path = body.get("folder_path").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing folder_path".to_string()))?;
    if folder_path.is_empty() || folder_path.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "Invalid folder_path".to_string()));
    }
    let vault_path = get_vault_path(&state, &vault_id).await?;
    std::fs::create_dir_all(std::path::Path::new(&vault_path).join(folder_path))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn rename_folder_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let folder_path = body.get("folder_path").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing folder_path".to_string()))?.to_string();
    let new_name = body.get("new_name").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing new_name".to_string()))?.trim().to_string();

    if folder_path.is_empty() || folder_path.contains("..") || new_name.is_empty() || new_name.contains('/') || new_name.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "Invalid folder_path or new_name".to_string()));
    }

    let parent = std::path::Path::new(&folder_path).parent()
        .and_then(|p| { let s = p.to_string_lossy().to_string(); if s.is_empty() { None } else { Some(s) } });
    let new_folder_path = match parent { Some(p) => format!("{}/{}", p, new_name), None => new_name.clone() };
    if new_folder_path == folder_path {
        return Ok(Json(json!({ "new_folder_path": folder_path })));
    }

    let vault_path = get_vault_path(&state, &vault_id).await?;
    std::fs::rename(
        std::path::Path::new(&vault_path).join(&folder_path),
        std::path::Path::new(&vault_path).join(&new_folder_path),
    ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Rename folder failed: {}", e)))?;

    let old_prefix = format!("{}/", folder_path);
    let new_prefix = format!("{}/", new_folder_path);

    #[derive(serde::Deserialize)]
    struct NotePathRow { path: String }

    if let Ok(mut resp) = state.db
        .query("SELECT path FROM notes WHERE vault_id = $vid AND string::starts_with(path, $prefix)")
        .bind(("vid", vault_id.clone())).bind(("prefix", old_prefix.clone())).await
    {
        let rows: Vec<NotePathRow> = resp.take(0).unwrap_or_default();
        for row in rows {
            let new_note_path = format!("{}{}", new_prefix, &row.path[old_prefix.len()..]);
            let abs_new_note = std::path::Path::new(&vault_path).join(&new_note_path);
            if let Ok(content) = std::fs::read_to_string(&abs_new_note) {
                let title = extract_title_from_content(&content, &new_note_path);
                let word_count = content.split_whitespace().count() as i64;
                let now = Utc::now().timestamp();
                let _ = state.db.query("DELETE FROM notes WHERE vault_id = $vid AND path = $old_path")
                    .bind(("vid", vault_id.clone())).bind(("old_path", row.path.clone())).await;
                let _ = state.db.query("INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at) VALUES ($vid, $path, $title, $content, $wc, $now, $now)")
                    .bind(("vid", vault_id.clone())).bind(("path", new_note_path.clone()))
                    .bind(("title", title)).bind(("content", content.clone()))
                    .bind(("wc", word_count)).bind(("now", now)).await;
                let _ = state.db.query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
                    .bind(("vid", vault_id.clone())).bind(("fp", row.path.clone())).await;
                index_note_chunks(&state, &vault_id, &new_note_path, &content).await;
            }
        }
    }
    Ok(Json(json!({ "new_folder_path": new_folder_path })))
}

pub(super) async fn rename_note_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let old_path = body.get("old_path").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing old_path".to_string()))?.to_string();
    let new_title = body.get("new_title").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing new_title".to_string()))?;

    if old_path.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "Invalid path".to_string()));
    }

    let parent = std::path::Path::new(&old_path).parent()
        .and_then(|p| { let s = p.to_string_lossy().to_string(); if s.is_empty() { None } else { Some(s) } });
    let safe_title: String = new_title.chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let new_filename = format!("{}.md", safe_title.trim());
    let new_path = match parent { Some(p) => format!("{}/{}", p, new_filename), None => new_filename };

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_old = std::path::Path::new(&vault_path).join(&old_path);
    let abs_new = std::path::Path::new(&vault_path).join(&new_path);

    if !abs_old.exists() {
        return Err((StatusCode::NOT_FOUND, format!("Note not found: {}", old_path)));
    }
    if let Some(parent) = abs_new.parent() {
        std::fs::create_dir_all(parent).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    std::fs::rename(&abs_old, &abs_new)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Rename failed: {}", e)))?;

    let _ = state.db.query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
        .bind(("vid", vault_id.clone())).bind(("path", old_path.clone())).await;
    let _ = state.db.query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
        .bind(("vid", vault_id.clone())).bind(("fp", old_path.clone())).await;
    {
        let sqlite = state.daemon.sqlite.clone();
        let vid = vault_id.clone(); let fp = old_path.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(conn) = sqlite.lock() { let _ = crate::db::sqlite::fts_delete_file(&conn, &vid, &fp); }
        });
    }

    if let Ok(content) = std::fs::read_to_string(&abs_new) {
        let title = extract_title_from_content(&content, &new_path);
        let word_count = content.split_whitespace().count() as i64;
        let now = Utc::now().timestamp();
        let _ = state.db
            .query("INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at) VALUES ($vid, $path, $title, $content, $wc, $now, $now) ON DUPLICATE KEY UPDATE title = $title, content = $content, word_count = $wc, modified_at = $now")
            .bind(("vid", vault_id.clone())).bind(("path", new_path.clone()))
            .bind(("title", title)).bind(("content", content.clone()))
            .bind(("wc", word_count)).bind(("now", now)).await;
        index_note_chunks(&state, &vault_id, &new_path, &content).await;
    }
    Ok(Json(json!({ "new_path": new_path })))
}

pub(super) async fn list_folders_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let mut folders: Vec<String> = Vec::new();
    collect_dirs(std::path::Path::new(&vault_path), std::path::Path::new(&vault_path), &mut folders);
    Ok(Json(json!(folders)))
}

fn collect_dirs(vault_root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') || name == "assets" { continue; }
        if let Ok(rel) = path.strip_prefix(vault_root) {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if !rel_str.is_empty() { out.push(rel_str); }
            collect_dirs(vault_root, &path, out);
        }
    }
}
