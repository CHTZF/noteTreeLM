mod crud;
mod trash;
mod folders;
mod assets;

use axum::{
    routing::{delete, get, patch, post},
    Router,
};
use serde::Deserialize;
use crate::app_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/vaults/:vault_id/notes",
            get(crud::list_notes).post(crud::create_note).put(crud::update_note).delete(crud::delete_note),
        )
        .route("/vaults/:vault_id/notes/read",    get(crud::read_note))
        .route("/vaults/:vault_id/notes/rename",  post(folders::rename_note_handler))
        .route("/vaults/:vault_id/notes/trash",   delete(trash::trash_note_handler))
        .route("/vaults/:vault_id/notes/status",  patch(crud::set_note_status_handler))
        .route("/vaults/:vault_id/folders",       post(folders::create_folder_handler))
        .route("/vaults/:vault_id/folders/list",  get(folders::list_folders_handler))
        .route("/vaults/:vault_id/folders/rename", post(folders::rename_folder_handler))
        .route("/vaults/:vault_id/folders/trash", delete(trash::trash_folder_handler))
        .route("/vaults/:vault_id/assets/list",   get(assets::list_assets_handler))
        .route("/vaults/:vault_id/assets",        delete(assets::delete_asset_handler))
        .route("/vaults/:vault_id/assets/import", post(assets::import_asset_handler))
        .route("/vaults/:vault_id/assets/rename", post(assets::rename_asset_handler))
        .route(
            "/vaults/:vault_id/trash",
            get(trash::list_trash_handler).delete(trash::delete_trash_items_handler),
        )
        .route("/vaults/:vault_id/trash/restore", post(trash::restore_trash_item_handler))
}

// ── Shared query / helper types ───────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct PathQuery {
    pub path: Option<String>,
}

// ── Shared helpers used by multiple submodules ────────────────────────────────

pub(super) fn extract_title_from_content(content: &str, fallback_path: &str) -> String {
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

/// Chunk, embed, and index a note into SurrealDB chunks + SQLite FTS5.
/// Best-effort: errors are logged but not propagated.
pub(super) async fn index_note_chunks(
    state: &crate::app_state::ApiState,
    vault_id: &str,
    rel_path: &str,
    content: &str,
) {
    let chunks = crate::embedding::chunker::split_into_chunks(content, rel_path);
    if chunks.is_empty() { return; }

    let current_ids: Vec<String> = chunks.iter().map(|c| c.chunk_id.clone()).collect();

    #[derive(serde::Deserialize)]
    struct ChunkIdRow { chunk_id: String }

    if let Ok(mut resp) = state.db
        .query("SELECT chunk_id FROM chunks WHERE vault_id = $vid AND file_path = $fp")
        .bind(("vid", vault_id.to_owned()))
        .bind(("fp", rel_path.to_owned()))
        .await
    {
        let existing: Vec<ChunkIdRow> = resp.take(0).unwrap_or_default();
        for row in existing {
            if !current_ids.contains(&row.chunk_id) {
                let _ = state.db
                    .query("DELETE FROM chunks WHERE vault_id = $vid AND chunk_id = $cid")
                    .bind(("vid", vault_id.to_owned()))
                    .bind(("cid", row.chunk_id))
                    .await;
            }
        }
    }

    let emb_url = Some(state.daemon.embedding_url.clone());
    let http_client = reqwest::Client::new();

    for chunk in &chunks {
        let embedding = crate::embedding::embedder::embed_text(
            &http_client, &emb_url,
            &crate::embedding::chunker::clean_for_embedding(&chunk.content),
        ).await;

        let _ = state.db
            .query("DELETE FROM chunks WHERE vault_id = $vid AND chunk_id = $cid")
            .bind(("vid", vault_id.to_owned()))
            .bind(("cid", chunk.chunk_id.clone()))
            .await;

        if let Some(ref vec) = embedding {
            let _ = state.db.query(
                "INSERT INTO chunks (vault_id, chunk_id, file_path, section, content, \
                 chunk_type, word_count, updated_at, embedding, status) \
                 VALUES ($vid, $cid, $fp, $section, $content, \
                 'text', $wc, time::now(), $emb, $status)",
            )
            .bind(("vid", vault_id.to_owned()))
            .bind(("cid", chunk.chunk_id.clone()))
            .bind(("fp",  chunk.file_path.clone()))
            .bind(("section", chunk.section.clone()))
            .bind(("content", chunk.content.clone()))
            .bind(("wc", chunk.word_count as i64))
            .bind(("emb", vec.clone()))
            .bind(("status", chunk.status.clone()))
            .await;
        } else {
            let _ = state.db.query(
                "INSERT INTO chunks (vault_id, chunk_id, file_path, section, content, \
                 chunk_type, word_count, updated_at, status) \
                 VALUES ($vid, $cid, $fp, $section, $content, \
                 'text', $wc, time::now(), $status)",
            )
            .bind(("vid", vault_id.to_owned()))
            .bind(("cid", chunk.chunk_id.clone()))
            .bind(("fp",  chunk.file_path.clone()))
            .bind(("section", chunk.section.clone()))
            .bind(("content", chunk.content.clone()))
            .bind(("wc", chunk.word_count as i64))
            .bind(("status", chunk.status.clone()))
            .await;
        }

        {
            let sqlite = state.daemon.sqlite.clone();
            let vid  = vault_id.to_owned();
            let cid  = chunk.chunk_id.clone();
            let fp   = chunk.file_path.clone();
            let sec  = chunk.section.clone();
            let cont = chunk.content.clone();
            let stat = chunk.status.clone();
            tokio::task::spawn_blocking(move || {
                if let Ok(conn) = sqlite.lock() {
                    if let Err(e) = crate::db::sqlite::fts_upsert(&conn, &cid, &vid, &fp, &sec, &cont, &stat) {
                        tracing::warn!("SQLite FTS upsert failed for {}: {}", cid, e);
                    }
                }
            });
        }
    }
}
