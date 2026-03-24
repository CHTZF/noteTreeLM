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
