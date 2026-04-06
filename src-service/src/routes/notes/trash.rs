use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::app_state::ApiState;
use crate::routes::vault::get_vault_path;
use super::{PathQuery, extract_title_from_content, index_note_chunks};

#[derive(Deserialize)]
pub(super) struct DeleteTrashBody {
    item_ids: Vec<String>,
}

pub(super) async fn trash_note_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = q.path.ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?;
    if rel_path.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "Invalid path".to_string()));
    }

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_path   = std::path::Path::new(&vault_path).join(&rel_path);
    let trash_dir  = std::path::PathBuf::from(&vault_path).join(".trash");
    std::fs::create_dir_all(&trash_dir).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let filename = std::path::Path::new(&rel_path)
        .file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "note.md".to_string());
    let title = if abs_path.exists() {
        std::fs::read_to_string(&abs_path).ok()
            .and_then(|c| c.lines().find(|l| l.trim_start().starts_with("# "))
                .map(|l| l.trim_start_matches('#').trim().to_string()))
            .unwrap_or_else(|| filename.trim_end_matches(".md").to_string())
    } else {
        filename.trim_end_matches(".md").to_string()
    };

    let trash_filename = if trash_dir.join(&filename).exists() {
        let stem = std::path::Path::new(&filename).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let ext  = std::path::Path::new(&filename).extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "md".to_string());
        format!("{}_{}.{}", stem, Utc::now().timestamp_millis(), ext)
    } else { filename.clone() };

    if abs_path.exists() {
        std::fs::rename(&abs_path, trash_dir.join(&trash_filename))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Move to trash failed: {}", e)))?;
    }

    let item_id = Uuid::new_v4().to_string();
    let meta = json!({
        "item_id": item_id, "original_path": rel_path, "name": filename,
        "title": title, "trash_filename": trash_filename,
        "deleted_at": Utc::now().timestamp_millis(),
    });
    let _ = std::fs::write(trash_dir.join(format!("{}.meta.json", trash_filename)), meta.to_string());

    let _ = state.db.query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
        .bind(("vid", vault_id.clone())).bind(("path", rel_path.clone())).await;
    let _ = state.db.query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
        .bind(("vid", vault_id.clone())).bind(("fp", rel_path.clone())).await;
    {
        let sqlite = state.daemon.sqlite.clone();
        let vid = vault_id.clone(); let fp = rel_path.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(conn) = sqlite.lock() { let _ = crate::db::sqlite::fts_delete_file(&conn, &vid, &fp); }
        });
    }
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn delete_trash_items_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<DeleteTrashBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let trash_dir  = std::path::PathBuf::from(&vault_path).join(".trash");

    for id in &body.item_ids {
        if let Ok(entries) = std::fs::read_dir(&trash_dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if !fname.ends_with(".meta.json") { continue; }
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(meta) = serde_json::from_str::<Value>(&content) {
                        if meta["item_id"].as_str() == Some(id.as_str()) {
                            let trash_filename = meta["trash_filename"].as_str().unwrap_or("");
                            let _ = std::fs::remove_file(trash_dir.join(trash_filename));
                            let _ = std::fs::remove_file(entry.path());
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn list_trash_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let trash_dir  = std::path::PathBuf::from(&vault_path).join(".trash");

    let mut items: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&trash_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if !fname.ends_with(".meta.json") { continue; }
            if let Ok(raw) = std::fs::read_to_string(entry.path()) {
                if let Ok(meta) = serde_json::from_str::<Value>(&raw) {
                    items.push(json!({
                        "id": meta["item_id"].as_str().unwrap_or(""),
                        "original_path": meta["original_path"].as_str().unwrap_or(""),
                        "name": meta["name"].as_str().unwrap_or(""),
                        "title": meta["title"].as_str().unwrap_or(""),
                        "trash_filename": meta["trash_filename"].as_str().unwrap_or(""),
                        "deleted_at": meta["deleted_at"].as_i64().unwrap_or(0),
                    }));
                }
            }
        }
    }
    items.sort_by(|a, b| b["deleted_at"].as_i64().unwrap_or(0).cmp(&a["deleted_at"].as_i64().unwrap_or(0)));
    Ok(Json(json!(items)))
}

pub(super) async fn restore_trash_item_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = body.get("id").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing id".to_string()))?.to_string();
    let target_folder = body.get("target_folder").and_then(|v| v.as_str())
        .unwrap_or("").trim_end_matches('/').to_string();

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let trash_dir  = std::path::PathBuf::from(&vault_path).join(".trash");

    let mut found_meta: Option<Value> = None;
    let mut found_meta_path: Option<std::path::PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(&trash_dir) {
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().ends_with(".meta.json") { continue; }
            if let Ok(raw) = std::fs::read_to_string(entry.path()) {
                if let Ok(meta) = serde_json::from_str::<Value>(&raw) {
                    if meta["item_id"].as_str() == Some(id.as_str()) {
                        found_meta = Some(meta);
                        found_meta_path = Some(entry.path());
                        break;
                    }
                }
            }
        }
    }

    let meta      = found_meta.ok_or((StatusCode::NOT_FOUND, "Trash item not found".to_string()))?;
    let meta_path = found_meta_path.unwrap();
    let trash_filename = meta["trash_filename"].as_str().unwrap_or("").to_string();
    let item_name      = meta["name"].as_str().unwrap_or(&trash_filename).to_string();

    let candidate = if target_folder.is_empty() { item_name.clone() }
                    else { format!("{}/{}", target_folder, item_name) };
    let new_path = if std::path::Path::new(&vault_path).join(&candidate).exists() {
        let stem = std::path::Path::new(&item_name).file_stem()
            .map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let suffixed = format!("{}_{}.md", stem, Utc::now().timestamp_millis());
        if target_folder.is_empty() { suffixed } else { format!("{}/{}", target_folder, suffixed) }
    } else { candidate };

    let abs_new = std::path::Path::new(&vault_path).join(&new_path);
    if let Some(parent) = abs_new.parent() {
        std::fs::create_dir_all(parent).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    std::fs::rename(trash_dir.join(&trash_filename), &abs_new)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Restore failed: {}", e)))?;
    let _ = std::fs::remove_file(&meta_path);

    if let Ok(content) = std::fs::read_to_string(&abs_new) {
        let title = extract_title_from_content(&content, &new_path);
        let word_count = content.split_whitespace().count() as i64;
        let now = Utc::now().timestamp();
        let _ = state.db
            .query("INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at) VALUES ($vid, $path, $title, $content, $wc, $now, $now)")
            .bind(("vid", vault_id.clone())).bind(("path", new_path.clone()))
            .bind(("title", title)).bind(("content", content.clone()))
            .bind(("wc", word_count)).bind(("now", now)).await;
        index_note_chunks(&state, &vault_id, &new_path, &content).await;
    }
    Ok(Json(json!({ "new_path": new_path })))
}

pub(super) async fn trash_folder_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let folder_path = q.path.ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?;
    if folder_path.is_empty() || folder_path.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "Invalid folder_path".to_string()));
    }

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let prefix = format!("{}/", folder_path.trim_end_matches('/'));
    let mut md_files: Vec<String> = Vec::new();
    collect_md_under_folder(
        std::path::Path::new(&vault_path), std::path::Path::new(&vault_path),
        &prefix, &mut md_files,
    );
    let count = md_files.len() as u32;

    let trash_dir = std::path::PathBuf::from(&vault_path).join(".trash");
    std::fs::create_dir_all(&trash_dir).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    for note_path in &md_files {
        let abs_path = std::path::Path::new(&vault_path).join(note_path);
        let content  = std::fs::read_to_string(&abs_path).unwrap_or_default();
        let title    = extract_title_from_content(&content, note_path);
        let filename = std::path::Path::new(note_path).file_name()
            .map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "note.md".to_string());
        let trash_filename = if trash_dir.join(&filename).exists() {
            let stem = std::path::Path::new(&filename).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            format!("{}_{}.md", stem, Utc::now().timestamp_millis())
        } else { filename.clone() };

        if abs_path.exists() { let _ = std::fs::rename(&abs_path, trash_dir.join(&trash_filename)); }
        let meta = json!({
            "item_id": Uuid::new_v4().to_string(), "original_path": note_path, "name": filename,
            "title": title, "trash_filename": trash_filename, "deleted_at": Utc::now().timestamp_millis(),
        });
        let _ = std::fs::write(trash_dir.join(format!("{}.meta.json", trash_filename)), meta.to_string());

        let _ = state.db.query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
            .bind(("vid", vault_id.clone())).bind(("path", note_path.clone())).await;
        let _ = state.db.query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
            .bind(("vid", vault_id.clone())).bind(("fp", note_path.clone())).await;
        {
            let sqlite = state.daemon.sqlite.clone();
            let vid = vault_id.clone(); let fp = note_path.clone();
            tokio::task::spawn_blocking(move || {
                if let Ok(conn) = sqlite.lock() { let _ = crate::db::sqlite::fts_delete_file(&conn, &vid, &fp); }
            });
        }
    }

    let abs_folder = std::path::Path::new(&vault_path).join(&folder_path);
    if abs_folder.exists() {
        std::fs::remove_dir_all(&abs_folder)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Remove folder failed: {}", e)))?;
    }
    Ok(Json(json!({ "ok": true, "count": count })))
}

fn collect_md_under_folder(vault_root: &std::path::Path, dir: &std::path::Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') { continue; }
        if path.is_dir() {
            collect_md_under_folder(vault_root, &path, prefix, out);
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            if let Ok(rel) = path.strip_prefix(vault_root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if rel_str.starts_with(prefix) { out.push(rel_str); }
            }
        }
    }
}
