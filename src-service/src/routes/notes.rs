use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api_state::ApiState;
use crate::routes::vault::get_vault_path;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/vaults/:vault_id/notes",
            get(list_notes).post(create_note).put(update_note).delete(delete_note),
        )
        .route("/vaults/:vault_id/notes/read", get(read_note))
        .route("/vaults/:vault_id/notes/trash", delete(trash_note_handler))
        .route("/vaults/:vault_id/notes/status", patch(set_note_status_handler))
        .route("/vaults/:vault_id/trash", delete(delete_trash_items_handler))
        .route("/vaults/:vault_id/assets", delete(delete_asset_handler))
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
    let rows: Vec<Value> = if let Some(prefix) = q.path_prefix.filter(|s| !s.is_empty()) {
        let mut resp = state
            .db
            .query("SELECT vault_id, path, title, word_count, modified_at FROM notes WHERE vault_id = $vid AND string::starts_with(path, $prefix) ORDER BY modified_at DESC")
            .bind(("vid", vault_id))
            .bind(("prefix", prefix))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        let mut resp = state
            .db
            .query("SELECT vault_id, path, title, word_count, modified_at FROM notes WHERE vault_id = $vid ORDER BY modified_at DESC")
            .bind(("vid", vault_id))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

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
    let chunks = crate::chunker::split_into_chunks(content, rel_path);
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
        let embedding = crate::embedder::embed_text(
            &http_client,
            &emb_url,
            &crate::chunker::clean_for_embedding(&chunk.content),
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
        "id": item_id,
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
