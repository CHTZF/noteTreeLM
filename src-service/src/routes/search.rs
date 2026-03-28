use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

use crate::api_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/vaults/:vault_id/search", get(search_notes))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

#[derive(Debug)]
struct SearchResult {
    #[allow(dead_code)]
    chunk_id:  String,
    file_path: String,
    section:   String,
    #[allow(dead_code)]
    content:   String,
    score:     f32,
}

async fn search_notes(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let query_str = q
        .q
        .filter(|s| !s.is_empty())
        .ok_or((StatusCode::BAD_REQUEST, "Missing query param q".to_string()))?;

    let mut results: Vec<SearchResult> = Vec::new();
    let mut seen_chunk_ids: HashSet<String> = HashSet::new();

    // 1. Try vector search if embedding server is available
    let emb_url = state.daemon.embedding_url.read().await.clone();
    if emb_url.is_some() {
        let http_client = reqwest::Client::new();
        if let Some(embedding) =
            crate::embedder::embed_text(&http_client, &emb_url, &query_str).await
        {
            #[derive(serde::Deserialize)]
            struct VecRow {
                chunk_id:  String,
                file_path: String,
                section:   String,
                content:   String,
                score:     f64,
            }

            let vec_resp = state
                .db
                .query(
                    "SELECT chunk_id, file_path, section, content, \
                     vector::similarity::cosine(embedding, $vec) AS score \
                     FROM chunks \
                     WHERE vault_id = $vid AND embedding IS NOT NONE \
                     ORDER BY score DESC LIMIT 20",
                )
                .bind(("vid", vault_id.clone()))
                .bind(("vec", embedding))
                .await;

            if let Ok(mut resp) = vec_resp {
                let rows: Vec<VecRow> = resp.take(0).unwrap_or_default();
                for row in rows {
                    if seen_chunk_ids.insert(row.chunk_id.clone()) {
                        results.push(SearchResult {
                            chunk_id:  row.chunk_id,
                            file_path: row.file_path,
                            section:   row.section,
                            content:   row.content,
                            score:     row.score as f32,
                        });
                    }
                }
            }
        }
    }

    // 2. FTS5 search (always run as fallback/supplement)
    {
        let sqlite = state.daemon.sqlite.clone();
        let vid = vault_id.clone();
        let query_clone = query_str.clone();
        let fts_results = tokio::task::spawn_blocking(move || {
            match sqlite.lock() {
                Ok(conn) => {
                    crate::db::sqlite::fts_search(&conn, &vid, &query_clone, false, 20)
                        .unwrap_or_default()
                }
                Err(_) => Vec::new(),
            }
        })
        .await
        .unwrap_or_default();

        for row in fts_results {
            if seen_chunk_ids.insert(row.chunk_id.clone()) {
                results.push(SearchResult {
                    chunk_id:  row.chunk_id,
                    file_path: row.file_path,
                    section:   row.section,
                    content:   row.content,
                    score:     0.5, // FTS results get a default score
                });
            }
        }
    }

    // 3. If results still empty, fallback to SurrealDB BM25
    if results.is_empty() {
        #[derive(serde::Deserialize)]
        struct BM25Row {
            path:  String,
            title: String,
            score: Option<f64>,
        }

        let bm25_resp = state
            .db
            .query(
                "SELECT path, title, search::score(1) AS score \
                 FROM notes \
                 WHERE vault_id = $vid AND (title @1@ $q OR content @1@ $q) \
                 ORDER BY score DESC LIMIT 20",
            )
            .bind(("vid", vault_id.clone()))
            .bind(("q", query_str.clone()))
            .await;

        if let Ok(mut resp) = bm25_resp {
            let rows: Vec<BM25Row> = resp.take(0).unwrap_or_default();
            let mut out: Vec<Value> = Vec::new();
            for r in rows {
                let source_url: Option<String> = if r.path.starts_with("imports/") {
                    #[derive(serde::Deserialize)]
                    struct UrlRow { url: String }
                    state.db
                        .query("SELECT url FROM import_pages WHERE vault_id = $vid AND note_path = $p LIMIT 1")
                        .bind(("vid", vault_id.clone()))
                        .bind(("p", r.path.clone()))
                        .await
                        .ok()
                        .and_then(|mut rr| rr.take::<Vec<UrlRow>>(0).ok())
                        .and_then(|v| v.into_iter().next().map(|row| row.url))
                } else { None };
                out.push(json!({
                    "path": r.path,
                    "title": r.title,
                    "section": "",
                    "score": r.score.unwrap_or(0.0),
                    "source_url": source_url,
                }));
            }
            return Ok(Json(json!(out)));
        }
    }

    // 4. Collect unique file_paths and load note metadata
    let unique_paths: Vec<String> = {
        let mut seen = HashSet::new();
        results
            .iter()
            .filter(|r| seen.insert(r.file_path.clone()))
            .map(|r| r.file_path.clone())
            .collect()
    };

    #[derive(serde::Deserialize)]
    struct NoteRow {
        path:  String,
        title: String,
    }

    let mut title_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut source_url_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for path in &unique_paths {
        if let Ok(mut resp) = state
            .db
            .query("SELECT path, title FROM notes WHERE vault_id = $vid AND path = $p LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .bind(("p", path.clone()))
            .await
        {
            let rows: Vec<NoteRow> = resp.take(0).unwrap_or_default();
            if let Some(row) = rows.into_iter().next() {
                title_map.insert(row.path, row.title);
            }
        }
        // Enrich import pages with their original source URL
        if path.starts_with("imports/") {
            #[derive(serde::Deserialize)]
            struct UrlRow { url: String }
            if let Ok(mut resp) = state
                .db
                .query("SELECT url FROM import_pages WHERE vault_id = $vid AND note_path = $p LIMIT 1")
                .bind(("vid", vault_id.clone()))
                .bind(("p", path.clone()))
                .await
            {
                let rows: Vec<UrlRow> = resp.take(0).unwrap_or_default();
                if let Some(row) = rows.into_iter().next() {
                    source_url_map.insert(path.clone(), row.url);
                }
            }
        }
    }

    let out: Vec<Value> = results
        .into_iter()
        .map(|r| {
            let title = title_map
                .get(&r.file_path)
                .cloned()
                .unwrap_or_else(|| r.file_path.clone());
            let source_url = source_url_map.get(&r.file_path);
            json!({
                "path": r.file_path,
                "title": title,
                "section": r.section,
                "score": r.score,
                "source_url": source_url,
            })
        })
        .collect();

    Ok(Json(json!(out)))
}
