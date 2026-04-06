use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, patch, post},
    Json, Router,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::state::ApiState;
use crate::routes::vault::get_vault_path;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/vaults/:vault_id/notes",
            get(list_notes).post(create_note).put(update_note).delete(delete_note),
        )
        .route("/vaults/:vault_id/notes/read", get(read_note))
        .route("/vaults/:vault_id/notes/rename", post(rename_note_handler))
        .route("/vaults/:vault_id/folders/list", get(list_folders_handler))
        .route("/vaults/:vault_id/assets/list", get(list_assets_handler))
        .route("/vaults/:vault_id/notes/trash", delete(trash_note_handler))
        .route("/vaults/:vault_id/notes/status", patch(set_note_status_handler))
        .route("/vaults/:vault_id/folders", post(create_folder_handler))
        .route("/vaults/:vault_id/folders/rename", post(rename_folder_handler))
        .route("/vaults/:vault_id/folders/trash", delete(trash_folder_handler))
        .route("/vaults/:vault_id/trash", get(list_trash_handler).delete(delete_trash_items_handler))
        .route("/vaults/:vault_id/trash/restore", post(restore_trash_item_handler))
        .route("/vaults/:vault_id/assets", delete(delete_asset_handler))
        .route("/vaults/:vault_id/assets/import", post(import_asset_handler))
        .route("/vaults/:vault_id/assets/rename", post(rename_asset_handler))
}

#[derive(Deserialize)]
struct PathQuery {
    path: Option<String>,
}

#[derive(Deserialize)]
struct ListNotesQuery {
    path_prefix: Option<String>,
}

async fn list_notes(
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
        let mut resp = state
            .db
            .query("SELECT path, title, word_count, created_at, modified_at FROM notes WHERE vault_id = $vid AND string::starts_with(path, $prefix) ORDER BY modified_at DESC")
            .bind(("vid", vault_id))
            .bind(("prefix", prefix))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).unwrap_or_default()
    } else {
        let mut resp = state
            .db
            .query("SELECT path, title, word_count, created_at, modified_at FROM notes WHERE vault_id = $vid ORDER BY modified_at DESC")
            .bind(("vid", vault_id))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).unwrap_or_default()
    };

    // Return shape compatible with frontend Note type (content empty — not needed for listing)
    let out: Vec<Value> = rows.into_iter().map(|r| json!({
        "path": r.path,
        "title": r.title,
        "content": "",
        "word_count": r.word_count,
        "created_at": r.created_at,
        "modified_at": r.modified_at,
    })).collect();

    Ok(Json(json!(out)))
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

    let title = extract_title_from_content(&content, &rel_path);
    let word_count = content.split_whitespace().count() as i64;

    let meta = std::fs::metadata(&full_path).ok();
    let to_ms = |t: std::time::SystemTime| {
        t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
    };
    let modified_at = meta.as_ref()
        .and_then(|m| m.modified().ok())
        .map(to_ms)
        .unwrap_or(0);
    let created_at = meta.as_ref()
        .and_then(|m| m.created().ok())
        .map(to_ms)
        .unwrap_or(modified_at);

    // Extract YAML frontmatter if present
    let frontmatter = if content.starts_with("---") {
        content.strip_prefix("---")
            .and_then(|rest| rest.find("\n---").map(|end| rest[..end].trim().to_string()))
    } else {
        None
    };

    Ok(Json(json!({
        "path": rel_path,
        "title": title,
        "content": content,
        "frontmatter": frontmatter,
        "word_count": word_count,
        "created_at": created_at,
        "modified_at": modified_at,
    })))
}

async fn create_note(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Accept either { path, content } or { title, folder?, content }
    let rel_path = if let Some(p) = body.get("path").and_then(|v| v.as_str()) {
        p.to_string()
    } else if let Some(title) = body.get("title").and_then(|v| v.as_str()) {
        let safe_title: String = title.chars()
            .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' { c } else { '_' })
            .collect();
        let filename = format!("{}.md", safe_title.trim());
        let folder = body.get("folder").and_then(|v| v.as_str()).unwrap_or("").trim_end_matches('/').to_string();
        if folder.is_empty() { filename } else { format!("{}/{}", folder, filename) }
    } else {
        return Err((StatusCode::BAD_REQUEST, "Missing path or title".to_string()));
    };
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
        .bind(("vid", vault_id.clone()))
        .bind(("path", rel_path.clone()))
        .bind(("title", title))
        .bind(("content", content.clone()))
        .bind(("wc", word_count))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Best-effort: chunk, embed, and index
    index_note_chunks(&state, &vault_id, &rel_path, &content).await;

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
        .bind(("content", content.clone()))
        .bind(("wc", word_count))
        .bind(("now", now))
        .bind(("vid", vault_id.clone()))
        .bind(("path", rel_path.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Best-effort: re-chunk, embed, and index (deletes stale chunks first)
    index_note_chunks(&state, &vault_id, &rel_path, &content).await;

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
        .bind(("vid", vault_id.clone()))
        .bind(("path", rel_path.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Delete chunks from SurrealDB
    let _ = state
        .db
        .query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
        .bind(("vid", vault_id.clone()))
        .bind(("fp", rel_path.clone()))
        .await;

    // Delete from SQLite FTS5 (best-effort)
    {
        let sqlite = state.daemon.sqlite.clone();
        let vid = vault_id.clone();
        let fp = rel_path.clone();
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

// ── Index helpers ─────────────────────────────────────────────────────────

/// Chunk, embed, and index a note into SurrealDB chunks + SQLite FTS5.
/// This is best-effort: errors are logged but not returned to the caller.
async fn index_note_chunks(state: &ApiState, vault_id: &str, rel_path: &str, content: &str) {
    let chunks = crate::processing::chunker::split_into_chunks(content, rel_path);
    if chunks.is_empty() {
        return;
    }

    // Collect current chunk IDs for this file
    let current_ids: Vec<String> = chunks.iter().map(|c| c.chunk_id.clone()).collect();

    // Delete stale chunks from SurrealDB (chunks no longer in the current set)
    #[derive(serde::Deserialize)]
    struct ChunkIdRow { chunk_id: String }

    if let Ok(mut resp) = state
        .db
        .query("SELECT chunk_id FROM chunks WHERE vault_id = $vid AND file_path = $fp")
        .bind(("vid", vault_id.to_owned()))
        .bind(("fp", rel_path.to_owned()))
        .await
    {
        let existing: Vec<ChunkIdRow> = resp.take(0).unwrap_or_default();
        for row in existing {
            if !current_ids.contains(&row.chunk_id) {
                let _ = state
                    .db
                    .query("DELETE FROM chunks WHERE vault_id = $vid AND chunk_id = $cid")
                    .bind(("vid", vault_id.to_owned()))
                    .bind(("cid", row.chunk_id))
                    .await;
            }
        }
    }

    // Get embedding URL
    let emb_url = state.daemon.embedding_url.read().await.clone();
    let http_client = reqwest::Client::new();

    for chunk in &chunks {
        // Try to get embedding
        let embedding = crate::processing::embedder::embed_text(
            &http_client,
            &emb_url,
            &crate::processing::chunker::clean_for_embedding(&chunk.content),
        )
        .await;

        // Delete existing chunk record then insert fresh (avoids SurrealDB FTS B-tree bug)
        let _ = state
            .db
            .query("DELETE FROM chunks WHERE vault_id = $vid AND chunk_id = $cid")
            .bind(("vid", vault_id.to_owned()))
            .bind(("cid", chunk.chunk_id.clone()))
            .await;

        if let Some(ref vec) = embedding {
            let _ = state
                .db
                .query(
                    "INSERT INTO chunks (vault_id, chunk_id, file_path, section, content, \
                     chunk_type, word_count, updated_at, embedding, status) \
                     VALUES ($vid, $cid, $fp, $section, $content, \
                     'text', $wc, time::now(), $emb, $status)",
                )
                .bind(("vid", vault_id.to_owned()))
                .bind(("cid", chunk.chunk_id.clone()))
                .bind(("fp", chunk.file_path.clone()))
                .bind(("section", chunk.section.clone()))
                .bind(("content", chunk.content.clone()))
                .bind(("wc", chunk.word_count as i64))
                .bind(("emb", vec.clone()))
                .bind(("status", chunk.status.clone()))
                .await;
        } else {
            let _ = state
                .db
                .query(
                    "INSERT INTO chunks (vault_id, chunk_id, file_path, section, content, \
                     chunk_type, word_count, updated_at, status) \
                     VALUES ($vid, $cid, $fp, $section, $content, \
                     'text', $wc, time::now(), $status)",
                )
                .bind(("vid", vault_id.to_owned()))
                .bind(("cid", chunk.chunk_id.clone()))
                .bind(("fp", chunk.file_path.clone()))
                .bind(("section", chunk.section.clone()))
                .bind(("content", chunk.content.clone()))
                .bind(("wc", chunk.word_count as i64))
                .bind(("status", chunk.status.clone()))
                .await;
        }

        // Best-effort: upsert into SQLite FTS5
        {
            let sqlite = state.daemon.sqlite.clone();
            let vid = vault_id.to_owned();
            let cid = chunk.chunk_id.clone();
            let fp = chunk.file_path.clone();
            let sec = chunk.section.clone();
            let cont = chunk.content.clone();
            let stat = chunk.status.clone();
            tokio::task::spawn_blocking(move || {
                if let Ok(conn) = sqlite.lock() {
                    if let Err(e) = crate::db::sqlite::fts_upsert(
                        &conn, &cid, &vid, &fp, &sec, &cont, &stat,
                    ) {
                        tracing::warn!("SQLite FTS upsert failed for {}: {}", cid, e);
                    }
                }
            });
        }
    }
}

// ── Trash handlers ───────────────────────────────────────────────────────────

async fn trash_note_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = q.path.ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?;
    if rel_path.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "Invalid path".to_string()));
    }

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_path = std::path::Path::new(&vault_path).join(&rel_path);

    let trash_dir = std::path::PathBuf::from(&vault_path).join(".trash");
    std::fs::create_dir_all(&trash_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let filename = std::path::Path::new(&rel_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "note.md".to_string());

    let title = if abs_path.exists() {
        std::fs::read_to_string(&abs_path)
            .ok()
            .and_then(|c| {
                c.lines()
                    .find(|l| l.trim_start().starts_with("# "))
                    .map(|l| l.trim_start_matches('#').trim().to_string())
            })
            .unwrap_or_else(|| filename.trim_end_matches(".md").to_string())
    } else {
        filename.trim_end_matches(".md").to_string()
    };

    // Avoid filename conflict in .trash/
    let trash_filename = if trash_dir.join(&filename).exists() {
        let stem = std::path::Path::new(&filename)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let ext = std::path::Path::new(&filename)
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_else(|| "md".to_string());
        format!("{}_{}.{}", stem, Utc::now().timestamp_millis(), ext)
    } else {
        filename.clone()
    };

    if abs_path.exists() {
        std::fs::rename(&abs_path, trash_dir.join(&trash_filename))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Move to trash failed: {}", e)))?;
    }

    // Write JSON sidecar (.trash/<trash_filename>.meta.json)
    let item_id = Uuid::new_v4().to_string();
    let now_ms = Utc::now().timestamp_millis();
    let meta = serde_json::json!({
        "item_id": item_id,
        "original_path": rel_path,
        "name": filename,
        "title": title,
        "trash_filename": trash_filename,
        "deleted_at": now_ms,
    });
    let meta_path = trash_dir.join(format!("{}.meta.json", trash_filename));
    let _ = std::fs::write(&meta_path, meta.to_string());

    // Remove from DB
    let _ = state.db
        .query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
        .bind(("vid", vault_id.clone()))
        .bind(("path", rel_path.clone()))
        .await;
    let _ = state.db
        .query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
        .bind(("vid", vault_id.clone()))
        .bind(("fp", rel_path.clone()))
        .await;

    // SQLite FTS delete (best-effort)
    {
        let sqlite = state.daemon.sqlite.clone();
        let vid = vault_id.clone();
        let fp = rel_path.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(conn) = sqlite.lock() {
                let _ = crate::db::sqlite::fts_delete_file(&conn, &vid, &fp);
            }
        });
    }

    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct DeleteTrashBody {
    item_ids: Vec<String>,
}

async fn delete_trash_items_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<DeleteTrashBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let trash_dir = std::path::PathBuf::from(&vault_path).join(".trash");

    for id in &body.item_ids {
        // Scan .meta.json sidecars to find the matching item by id
        if let Ok(entries) = std::fs::read_dir(&trash_dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if fname.ends_with(".meta.json") {
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
    }

    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SetStatusBody {
    path: String,
    status: String,
}

async fn set_note_status_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<SetStatusBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_path = std::path::Path::new(&vault_path).join(&body.path);

    let content = std::fs::read_to_string(&abs_path)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("Cannot read file: {}", e)))?;

    let updated = update_frontmatter_field(&content, "status", &body.status);

    std::fs::write(&abs_path, &updated)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Update DB record
    let now = Utc::now().timestamp();
    let title = extract_title_from_content(&updated, &body.path);
    let word_count = updated.split_whitespace().count() as i64;
    let _ = state.db
        .query("UPDATE notes SET title = $title, content = $content, word_count = $wc, modified_at = $now WHERE vault_id = $vid AND path = $path")
        .bind(("title", title))
        .bind(("content", updated.clone()))
        .bind(("wc", word_count))
        .bind(("now", now))
        .bind(("vid", vault_id.clone()))
        .bind(("path", body.path.clone()))
        .await;

    Ok(Json(json!({ "ok": true })))
}

/// Update or insert a key: value field in YAML frontmatter.
fn update_frontmatter_field(content: &str, key: &str, value: &str) -> String {
    if let Some(rest) = content.strip_prefix("---") {
        if let Some(fm_end) = rest.find("\n---") {
            let frontmatter = &rest[..fm_end];
            let after = &rest[fm_end + 4..]; // skip "\n---"
            let key_prefix = format!("{}:", key);
            let updated_fm = if frontmatter.lines().any(|l| l.trim_start().starts_with(&key_prefix)) {
                frontmatter.lines()
                    .map(|l| {
                        if l.trim_start().starts_with(&key_prefix) {
                            format!("{}: {}", key, value)
                        } else {
                            l.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                format!("{}\n{}: {}", frontmatter.trim_end(), key, value)
            };
            return format!("---{}---{}", updated_fm, after);
        }
    }
    // No frontmatter — prepend one
    format!("---\n{}: {}\n---\n\n{}", key, value, content)
}

// ── Rename note ──────────────────────────────────────────────────────────────

async fn rename_note_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let old_path = body
        .get("old_path")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing old_path".to_string()))?
        .to_string();
    let new_title = body
        .get("new_title")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing new_title".to_string()))?;

    if old_path.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "Invalid path".to_string()));
    }

    // Compute new path (keep same folder, change filename)
    let old_pathbuf = std::path::Path::new(&old_path);
    let parent = old_pathbuf
        .parent()
        .and_then(|p| {
            let s = p.to_string_lossy().to_string();
            if s.is_empty() { None } else { Some(s) }
        });
    let safe_title: String = new_title
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let new_filename = format!("{}.md", safe_title.trim());
    let new_path = match parent {
        Some(p) => format!("{}/{}", p, new_filename),
        None => new_filename,
    };

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_old = std::path::Path::new(&vault_path).join(&old_path);
    let abs_new = std::path::Path::new(&vault_path).join(&new_path);

    if !abs_old.exists() {
        return Err((StatusCode::NOT_FOUND, format!("Note not found: {}", old_path)));
    }
    if let Some(parent) = abs_new.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    std::fs::rename(&abs_old, &abs_new)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Rename failed: {}", e)))?;

    // Update DB: delete old record, re-index new path
    let _ = state
        .db
        .query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
        .bind(("vid", vault_id.clone()))
        .bind(("path", old_path.clone()))
        .await;
    let _ = state
        .db
        .query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
        .bind(("vid", vault_id.clone()))
        .bind(("fp", old_path.clone()))
        .await;
    {
        let sqlite = state.daemon.sqlite.clone();
        let vid = vault_id.clone();
        let fp = old_path.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(conn) = sqlite.lock() {
                let _ = crate::db::sqlite::fts_delete_file(&conn, &vid, &fp);
            }
        });
    }

    if let Ok(content) = std::fs::read_to_string(&abs_new) {
        let title = extract_title_from_content(&content, &new_path);
        let word_count = content.split_whitespace().count() as i64;
        let now = Utc::now().timestamp();
        let _ = state
            .db
            .query("INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at) VALUES ($vid, $path, $title, $content, $wc, $now, $now) ON DUPLICATE KEY UPDATE title = $title, content = $content, word_count = $wc, modified_at = $now")
            .bind(("vid", vault_id.clone()))
            .bind(("path", new_path.clone()))
            .bind(("title", title))
            .bind(("content", content.clone()))
            .bind(("wc", word_count))
            .bind(("now", now))
            .await;
        index_note_chunks(&state, &vault_id, &new_path, &content).await;
    }

    Ok(Json(json!({ "new_path": new_path })))
}

// ── Folder handlers ───────────────────────────────────────────────────────────

async fn create_folder_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let folder_path = body
        .get("folder_path")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing folder_path".to_string()))?;

    if folder_path.is_empty() || folder_path.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "Invalid folder_path".to_string()));
    }

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_path = std::path::Path::new(&vault_path).join(folder_path);
    std::fs::create_dir_all(&abs_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn rename_folder_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let folder_path = body
        .get("folder_path")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing folder_path".to_string()))?
        .to_string();
    let new_name = body
        .get("new_name")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing new_name".to_string()))?
        .trim()
        .to_string();

    if folder_path.is_empty() || folder_path.contains("..") || new_name.is_empty() || new_name.contains('/') || new_name.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "Invalid folder_path or new_name".to_string()));
    }

    let parent = std::path::Path::new(&folder_path)
        .parent()
        .and_then(|p| {
            let s = p.to_string_lossy().to_string();
            if s.is_empty() { None } else { Some(s) }
        });
    let new_folder_path = match parent {
        Some(p) => format!("{}/{}", p, new_name),
        None => new_name.clone(),
    };

    if new_folder_path == folder_path {
        return Ok(Json(json!({ "new_folder_path": folder_path })));
    }

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_old = std::path::Path::new(&vault_path).join(&folder_path);
    let abs_new = std::path::Path::new(&vault_path).join(&new_folder_path);

    std::fs::rename(&abs_old, &abs_new)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Rename folder failed: {}", e)))?;

    // Update all note paths in DB that start with old folder prefix
    let old_prefix = format!("{}/", folder_path);
    let new_prefix = format!("{}/", new_folder_path);

    // Fetch affected notes and re-index them
    #[derive(serde::Deserialize)]
    struct NotePathRow { path: String }

    if let Ok(mut resp) = state
        .db
        .query("SELECT path FROM notes WHERE vault_id = $vid AND string::starts_with(path, $prefix)")
        .bind(("vid", vault_id.clone()))
        .bind(("prefix", old_prefix.clone()))
        .await
    {
        let rows: Vec<NotePathRow> = resp.take(0).unwrap_or_default();
        for row in rows {
            let new_note_path = format!("{}{}", new_prefix, &row.path[old_prefix.len()..]);
            let abs_new_note = std::path::Path::new(&vault_path).join(&new_note_path);
            if let Ok(content) = std::fs::read_to_string(&abs_new_note) {
                let title = extract_title_from_content(&content, &new_note_path);
                let word_count = content.split_whitespace().count() as i64;
                let now = Utc::now().timestamp();
                let _ = state.db
                    .query("DELETE FROM notes WHERE vault_id = $vid AND path = $old_path")
                    .bind(("vid", vault_id.clone()))
                    .bind(("old_path", row.path.clone()))
                    .await;
                let _ = state.db
                    .query("INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at) VALUES ($vid, $path, $title, $content, $wc, $now, $now)")
                    .bind(("vid", vault_id.clone()))
                    .bind(("path", new_note_path.clone()))
                    .bind(("title", title))
                    .bind(("content", content.clone()))
                    .bind(("wc", word_count))
                    .bind(("now", now))
                    .await;
                let _ = state.db
                    .query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
                    .bind(("vid", vault_id.clone()))
                    .bind(("fp", row.path.clone()))
                    .await;
                index_note_chunks(&state, &vault_id, &new_note_path, &content).await;
            }
        }
    }

    Ok(Json(json!({ "new_folder_path": new_folder_path })))
}

// ── Trash list / restore ──────────────────────────────────────────────────────

async fn list_trash_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let trash_dir = std::path::PathBuf::from(&vault_path).join(".trash");

    let mut items: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&trash_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if !fname.ends_with(".meta.json") {
                continue;
            }
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

    // Sort by deleted_at descending
    items.sort_by(|a, b| {
        let ta = a["deleted_at"].as_i64().unwrap_or(0);
        let tb = b["deleted_at"].as_i64().unwrap_or(0);
        tb.cmp(&ta)
    });

    Ok(Json(json!(items)))
}

async fn restore_trash_item_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing id".to_string()))?
        .to_string();
    let target_folder = body
        .get("target_folder")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let trash_dir = std::path::PathBuf::from(&vault_path).join(".trash");

    // Find the .meta.json matching this id
    let mut found_meta: Option<Value> = None;
    let mut found_meta_path: Option<std::path::PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(&trash_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if !fname.ends_with(".meta.json") {
                continue;
            }
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

    let meta = found_meta.ok_or((StatusCode::NOT_FOUND, "Trash item not found".to_string()))?;
    let meta_path = found_meta_path.unwrap();

    let trash_filename = meta["trash_filename"].as_str().unwrap_or("").to_string();
    let item_name = meta["name"].as_str().unwrap_or(&trash_filename).to_string();

    let candidate = if target_folder.is_empty() {
        item_name.clone()
    } else {
        format!("{}/{}", target_folder, item_name)
    };

    // Avoid collision: add timestamp suffix if target exists
    let new_path = if std::path::Path::new(&vault_path).join(&candidate).exists() {
        let stem = std::path::Path::new(&item_name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let ts = Utc::now().timestamp_millis();
        let suffixed = format!("{}_{}.md", stem, ts);
        if target_folder.is_empty() { suffixed } else { format!("{}/{}", target_folder, suffixed) }
    } else {
        candidate
    };

    let trash_file = trash_dir.join(&trash_filename);
    let abs_new = std::path::Path::new(&vault_path).join(&new_path);
    if let Some(parent) = abs_new.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    std::fs::rename(&trash_file, &abs_new)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Restore failed: {}", e)))?;

    // Remove the .meta.json sidecar
    let _ = std::fs::remove_file(&meta_path);

    // Re-index restored note
    if let Ok(content) = std::fs::read_to_string(&abs_new) {
        let title = extract_title_from_content(&content, &new_path);
        let word_count = content.split_whitespace().count() as i64;
        let now = Utc::now().timestamp();
        let _ = state
            .db
            .query("INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at) VALUES ($vid, $path, $title, $content, $wc, $now, $now)")
            .bind(("vid", vault_id.clone()))
            .bind(("path", new_path.clone()))
            .bind(("title", title))
            .bind(("content", content.clone()))
            .bind(("wc", word_count))
            .bind(("now", now))
            .await;
        index_note_chunks(&state, &vault_id, &new_path, &content).await;
    }

    Ok(Json(json!({ "new_path": new_path })))
}

// ── Trash folder ─────────────────────────────────────────────────────────────

async fn trash_folder_handler(
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

    // Collect all .md files under the folder
    let mut md_files: Vec<String> = Vec::new();
    collect_md_under_folder(std::path::Path::new(&vault_path), std::path::Path::new(&vault_path), &prefix, &mut md_files);

    let count = md_files.len() as u32;

    // Trash each .md file (write .meta.json sidecar + move to .trash/)
    let trash_dir = std::path::PathBuf::from(&vault_path).join(".trash");
    std::fs::create_dir_all(&trash_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    for note_path in &md_files {
        let abs_path = std::path::Path::new(&vault_path).join(note_path);
        let content = std::fs::read_to_string(&abs_path).unwrap_or_default();
        let title = extract_title_from_content(&content, note_path);
        let filename = std::path::Path::new(note_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "note.md".to_string());

        let trash_filename = if trash_dir.join(&filename).exists() {
            let stem = std::path::Path::new(&filename)
                .file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            format!("{}_{}.md", stem, Utc::now().timestamp_millis())
        } else {
            filename.clone()
        };

        if abs_path.exists() {
            let _ = std::fs::rename(&abs_path, trash_dir.join(&trash_filename));
        }

        let item_id = Uuid::new_v4().to_string();
        let now_ms = Utc::now().timestamp_millis();
        let meta = serde_json::json!({
            "item_id": item_id,
            "original_path": note_path,
            "name": filename,
            "title": title,
            "trash_filename": trash_filename,
            "deleted_at": now_ms,
        });
        let _ = std::fs::write(
            trash_dir.join(format!("{}.meta.json", trash_filename)),
            meta.to_string(),
        );

        // Remove from DB
        let _ = state.db
            .query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
            .bind(("vid", vault_id.clone()))
            .bind(("path", note_path.clone()))
            .await;
        let _ = state.db
            .query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
            .bind(("vid", vault_id.clone()))
            .bind(("fp", note_path.clone()))
            .await;
        {
            let sqlite = state.daemon.sqlite.clone();
            let vid = vault_id.clone();
            let fp = note_path.clone();
            tokio::task::spawn_blocking(move || {
                if let Ok(conn) = sqlite.lock() {
                    let _ = crate::db::sqlite::fts_delete_file(&conn, &vid, &fp);
                }
            });
        }
    }

    // Delete the physical folder
    let abs_folder = std::path::Path::new(&vault_path).join(&folder_path);
    if abs_folder.exists() {
        std::fs::remove_dir_all(&abs_folder)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Remove folder failed: {}", e)))?;
    }

    Ok(Json(json!({ "ok": true, "count": count })))
}

fn collect_md_under_folder(
    vault_root: &std::path::Path,
    dir: &std::path::Path,
    prefix: &str,
    out: &mut Vec<String>,
) {
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
                if rel_str.starts_with(prefix) {
                    out.push(rel_str);
                }
            }
        }
    }
}

// ── Import / Rename asset ─────────────────────────────────────────────────────

async fn import_asset_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let filename = body.get("filename").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing filename".to_string()))?;
    let content_b64 = body.get("content_base64").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing content_base64".to_string()))?;
    let folder = body.get("folder").and_then(|v| v.as_str()).unwrap_or("").trim_end_matches('/').to_string();
    let new_name = body.get("new_name").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty());

    if filename.contains("..") || filename.contains('/') {
        return Err((StatusCode::BAD_REQUEST, "Invalid filename".to_string()));
    }

    // Decode base64 content
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content_b64)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid base64: {}", e)))?;

    // Determine final filename (apply new_name if provided, preserving extension)
    let final_filename = if let Some(name) = new_name {
        let name = name.trim().to_string();
        if std::path::Path::new(&name).extension().is_some() {
            name
        } else {
            let orig_ext = std::path::Path::new(filename)
                .extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            if orig_ext.is_empty() { name } else { format!("{}.{}", name, orig_ext) }
        }
    } else {
        filename.to_string()
    };

    let rel_path = if folder.is_empty() {
        final_filename.clone()
    } else {
        format!("{}/{}", folder, final_filename)
    };

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let dest = std::path::Path::new(&vault_path).join(&rel_path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    std::fs::write(&dest, &bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Write failed: {}", e)))?;

    Ok(Json(json!({ "rel_path": rel_path })))
}

async fn rename_asset_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let path = body.get("path").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?
        .to_string();
    let new_name = body.get("new_name").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing new_name".to_string()))?
        .trim().to_string();

    if path.contains("..") || new_name.contains("..") || new_name.contains('/') || new_name.contains('\\') {
        return Err((StatusCode::BAD_REQUEST, "Invalid path or new_name".to_string()));
    }

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_path = std::path::Path::new(&vault_path).join(&path);
    let parent = abs_path.parent()
        .ok_or((StatusCode::BAD_REQUEST, "Cannot get parent dir".to_string()))?;
    let new_abs_path = parent.join(&new_name);

    if new_abs_path.exists() {
        return Err((StatusCode::CONFLICT, format!("File {} already exists", new_name)));
    }

    std::fs::rename(&abs_path, &new_abs_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Rename failed: {}", e)))?;

    let new_rel = new_abs_path.strip_prefix(std::path::Path::new(&vault_path))
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Cannot compute new path".to_string()))?;

    Ok(Json(json!({ "new_path": new_rel })))
}

// ── Folder / Asset listing ────────────────────────────────────────────────────

async fn list_folders_handler(
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

async fn list_assets_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let mut assets: Vec<String> = Vec::new();
    collect_assets_fs(std::path::Path::new(&vault_path), std::path::Path::new(&vault_path), &mut assets);
    Ok(Json(json!(assets)))
}

fn collect_assets_fs(vault_root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') { continue; }
        if path.is_dir() {
            collect_assets_fs(vault_root, &path, out);
        } else {
            let ext = path.extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if matches!(ext.as_str(), "md" | "markdown" | "mdx") { continue; }
            if let Ok(rel) = path.strip_prefix(vault_root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if !rel_str.is_empty() { out.push(rel_str); }
            }
        }
    }
}

// ── Asset handler ─────────────────────────────────────────────────────────────

async fn delete_asset_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = q.path.ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?;
    if rel_path.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "Invalid path".to_string()));
    }
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_path = std::path::Path::new(&vault_path).join(&rel_path);
    if abs_path.exists() {
        std::fs::remove_file(&abs_path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Title / content helpers ───────────────────────────────────────────────────

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
