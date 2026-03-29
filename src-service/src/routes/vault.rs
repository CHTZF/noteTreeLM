use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path as FsPath;
use uuid::Uuid;

use crate::api_state::ApiState;
use crate::chunker;
use crate::embedder;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/vaults", get(list_vaults).post(register_vault))
        .route("/vaults/:vault_id/structure", get(vault_structure))
        .route("/vaults/:vault_id/scan", post(scan_vault))
        .route("/vaults/:vault_id/graph", get(get_graph))
        .route("/vaults/:vault_id/graph/related", get(get_graph_related))
        .route("/vaults/:vault_id/backlinks", get(get_backlinks))
        .route("/vaults/:vault_id/stats", get(get_vault_stats))
}

async fn list_vaults(
    State(state): State<ApiState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT *, record::id(id) AS id FROM vaults ORDER BY created_at DESC")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn register_vault(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let path = body
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?
        .to_string();

    // account_id: from body (legacy) or resolved from Bearer token
    let account_id = if let Some(aid) = body.get("account_id").and_then(|v| v.as_str()) {
        aid.to_string()
    } else if let Some(tok) = crate::routes::auth::extract_bearer(&headers) {
        resolve_account_id_from_token(&state, &tok).await
            .unwrap_or_else(|| body.get("account").and_then(|v| v.as_str()).unwrap_or("").to_string())
    } else {
        body.get("account").and_then(|v| v.as_str()).unwrap_or("").to_string()
    };

    let now = Utc::now().timestamp();

    // Check if vault with (path, account_id) already exists
    let mut check = state
        .db
        .query("SELECT vault_id FROM vaults WHERE path = $path AND account_id = $aid LIMIT 1")
        .bind(("path", path.clone()))
        .bind(("aid", account_id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let existing: Vec<Value> = check.take(0).unwrap_or_default();
    if let Some(existing_id) = existing.first().and_then(|r| r["vault_id"].as_str()) {
        let existing_id = existing_id.to_string();
        // Idempotent: ensure schedule exists even for vaults created before scheduling was added
        crate::seeds::ensure_memory_schedule(&state.db, &existing_id, &account_id).await;
        return Ok(Json(json!({ "vault_id": existing_id })));
    }

    let vault_id = Uuid::new_v4().to_string();
    state
        .db
        .query("INSERT INTO vaults (vault_id, path, account_id, created_at) VALUES ($vid, $path, $aid, $now)")
        .bind(("vid", vault_id.clone()))
        .bind(("path", path))
        .bind(("aid", account_id.clone()))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Ensure periodic memory_agent schedule exists for this vault+account
    crate::seeds::ensure_memory_schedule(&state.db, &vault_id, &account_id).await;

    Ok(Json(json!({ "vault_id": vault_id })))
}

async fn resolve_account_id_from_token(state: &ApiState, token: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Row { username: String }
    let now = chrono::Utc::now().timestamp();
    let mut r = state.db
        .query("SELECT username FROM sessions WHERE token = $t AND expires_at > $now LIMIT 1")
        .bind(("t", token.to_string()))
        .bind(("now", now))
        .await.ok()?;
    r.take::<Vec<Row>>(0).ok()?.into_iter().next().map(|r| r.username)
}

async fn vault_structure(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let tree = build_tree(FsPath::new(&vault_path), FsPath::new(&vault_path));
    Ok(Json(json!({ "vault_id": vault_id, "path": vault_path, "tree": tree })))
}

async fn scan_vault(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let root = FsPath::new(&vault_path);

    let md_files = collect_md_files(root);
    let now = Utc::now().timestamp();
    let mut indexed = 0usize;
    let mut indexed_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Get embedding URL once for the whole scan
    let emb_url = state.daemon.embedding_url.read().await.clone();
    let http_client = reqwest::Client::new();

    for file_path in md_files {
        let rel_path = file_path
            .strip_prefix(root)
            .unwrap_or(&file_path)
            .to_string_lossy()
            .to_string();

        let content = match tokio::fs::read_to_string(&file_path).await {
            Ok(c) => c,
            Err(_) => continue,
        };

        let title = extract_title(&content, &rel_path);
        let word_count = content.split_whitespace().count() as i64;
        let modified_at = file_path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(now);

        let _ = state
            .db
            .query("INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at) VALUES ($vid, $path, $title, $content, $wc, $now, $mod) ON DUPLICATE KEY UPDATE title = $title, content = $content, word_count = $wc, modified_at = $mod")
            .bind(("vid", vault_id.clone()))
            .bind(("path", rel_path.clone()))
            .bind(("title", title))
            .bind(("content", content.clone()))
            .bind(("wc", word_count))
            .bind(("now", now))
            .bind(("mod", modified_at))
            .await;

        // Chunk, embed, and index
        let chunks = chunker::split_into_chunks(&content, &rel_path);
        // Delete stale chunks for this file before inserting new ones
        let _ = state
            .db
            .query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
            .bind(("vid", vault_id.clone()))
            .bind(("fp", rel_path.clone()))
            .await;

        // Best-effort: clear from SQLite FTS5
        {
            let sqlite = state.daemon.sqlite.clone();
            let vid = vault_id.clone();
            let fp = rel_path.clone();
            tokio::task::spawn_blocking(move || {
                if let Ok(conn) = sqlite.lock() {
                    let _ = crate::db::sqlite::fts_delete_file(&conn, &vid, &fp);
                }
            })
            .await
            .ok();
        }

        for chunk in &chunks {
            let embedding = embedder::embed_text(
                &http_client,
                &emb_url,
                &chunker::clean_for_embedding(&chunk.content),
            )
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
                    .bind(("vid", vault_id.clone()))
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
                    .bind(("vid", vault_id.clone()))
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
                let vid = vault_id.clone();
                let cid = chunk.chunk_id.clone();
                let fp = chunk.file_path.clone();
                let sec = chunk.section.clone();
                let cont = chunk.content.clone();
                let stat = chunk.status.clone();
                tokio::task::spawn_blocking(move || {
                    if let Ok(conn) = sqlite.lock() {
                        if let Err(e) =
                            crate::db::sqlite::fts_upsert(&conn, &cid, &vid, &fp, &sec, &cont, &stat)
                        {
                            tracing::warn!("SQLite FTS upsert failed for {}: {}", cid, e);
                        }
                    }
                });
            }
        }

        // Extract and store wiki-links for this file
        {
            let wiki_links = extract_wiki_links(&content);
            let _ = state
                .db
                .query("DELETE FROM links WHERE vault_id = $vid AND source_path = $path")
                .bind(("vid", vault_id.clone()))
                .bind(("path", rel_path.clone()))
                .await;
            for (target_title, raw_text, line_number) in wiki_links {
                let link_id = Uuid::new_v4().to_string();
                let _ = state
                    .db
                    .query(
                        "INSERT INTO links (link_id, vault_id, source_path, target_title, \
                         link_type, raw_text, line_number) \
                         VALUES ($lid, $vid, $src, $title, 'wiki', $raw, $line)",
                    )
                    .bind(("lid", link_id))
                    .bind(("vid", vault_id.clone()))
                    .bind(("src", rel_path.clone()))
                    .bind(("title", target_title))
                    .bind(("raw", raw_text))
                    .bind(("line", line_number))
                    .await;
            }
        }

        // Upsert graph node for this note
        {
            let node_id = format!("{}:{}", vault_id, rel_path);
            let note_title = extract_title(&content, &rel_path);
            let _ = state
                .db
                .query(
                    "INSERT INTO graph_nodes (vault_id, node_id, node_type, label, file_path, created_at) \
                     VALUES ($vid, $nid, 'note', $label, $fp, $now) \
                     ON DUPLICATE KEY UPDATE label = $label, file_path = $fp",
                )
                .bind(("vid", vault_id.clone()))
                .bind(("nid", node_id))
                .bind(("label", note_title))
                .bind(("fp", rel_path.clone()))
                .bind(("now", now))
                .await;
        }

        indexed_paths.insert(rel_path.clone());
        indexed += 1;
    }

    // Rebuild graph edges from links table (resolve target_title → file_path via notes)
    {
        // Load all notes title→node_id map
        #[derive(serde::Deserialize)]
        struct NoteRow { title: String, path: String }
        let notes_res: surrealdb::Result<Vec<NoteRow>> = state
            .db
            .query("SELECT title, path FROM notes WHERE vault_id = $vid")
            .bind(("vid", vault_id.clone()))
            .await
            .and_then(|mut r| r.take(0));

        if let Ok(note_rows) = notes_res {
            let title_to_node: std::collections::HashMap<String, String> = note_rows
                .into_iter()
                .map(|n| (n.title.to_lowercase(), format!("{}:{}", vault_id, n.path)))
                .collect();

            // Delete existing wiki_link edges for this vault
            let _ = state
                .db
                .query("DELETE FROM graph_edges WHERE vault_id = $vid AND edge_type = 'wiki_link'")
                .bind(("vid", vault_id.clone()))
                .await;

            // Fetch all links for this vault
            #[derive(serde::Deserialize)]
            struct LinkRow { source_path: String, target_title: String }
            let links_res: surrealdb::Result<Vec<LinkRow>> = state
                .db
                .query("SELECT source_path, target_title FROM links WHERE vault_id = $vid")
                .bind(("vid", vault_id.clone()))
                .await
                .and_then(|mut r| r.take(0));

            if let Ok(link_rows) = links_res {
                for link in link_rows {
                    let source_id = format!("{}:{}", vault_id, link.source_path);
                    let target_key = link.target_title.to_lowercase();
                    if let Some(target_id) = title_to_node.get(&target_key) {
                        let edge_id = Uuid::new_v4().to_string();
                        let _ = state
                            .db
                            .query(
                                "INSERT INTO graph_edges (edge_id, vault_id, source_id, target_id, edge_type, weight) \
                                 VALUES ($eid, $vid, $src, $tgt, 'wiki_link', 1.0)",
                            )
                            .bind(("eid", edge_id))
                            .bind(("vid", vault_id.clone()))
                            .bind(("src", source_id))
                            .bind(("tgt", target_id.clone()))
                            .await;
                    }
                }
            }
        }
    }

    // Prune orphan note graph_nodes (deleted files no longer in notes table)
    {
        #[derive(serde::Deserialize)]
        struct NodeRow { node_id: String, file_path: Option<String> }
        let existing_nodes: Vec<NodeRow> = state
            .db
            .query("SELECT node_id, file_path FROM graph_nodes WHERE vault_id = $vid AND node_type = 'note'")
            .bind(("vid", vault_id.clone()))
            .await
            .and_then(|mut r| r.take(0))
            .unwrap_or_default();

        for node in existing_nodes {
            let fp = node.file_path.unwrap_or_default();
            // indexed_paths contains all rel_paths that still exist on disk
            if !indexed_paths.contains(&fp) {
                let _ = state
                    .db
                    .query("DELETE FROM graph_nodes WHERE node_id = $nid")
                    .bind(("nid", node.node_id))
                    .await;
            }
        }
    }

    state.daemon.emit("vault:changed", serde_json::json!({
        "vault_id": vault_id,
        "indexed": indexed,
    }));

    Ok(Json(json!({ "ok": true, "indexed": indexed })))
}

/// Extract [[wiki-links]] from markdown content.
/// Returns Vec<(target_title, raw_text, line_number)>
fn extract_wiki_links(content: &str) -> Vec<(String, String, i64)> {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"\[\[([^\]\[]+)\]\]").unwrap()
    });
    let mut results = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        for cap in RE.captures_iter(line) {
            let inner = &cap[1];
            // Strip alias (|) and heading (#)
            let target = inner.split('|').next()
                .and_then(|s| s.split('#').next())
                .unwrap_or("").trim().to_string();
            if !target.is_empty() {
                results.push((target, cap[0].to_string(), line_idx as i64 + 1));
            }
        }
    }
    results
}

// ── Graph ──────────────────────────────────────────────────────────────────

async fn get_graph(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Nodes: all notes in the vault
    let mut resp = state
        .db
        .query("SELECT path, title FROM notes WHERE vault_id = $vid")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| {
            tracing::error!("get_graph notes query failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    #[derive(serde::Deserialize)]
    struct NoteRow { path: String, title: String }
    let notes: Vec<NoteRow> = resp.take(0).map_err(|e| {
        tracing::error!("get_graph notes take failed: {}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    let nodes: Vec<Value> = notes
        .iter()
        .map(|n| json!({
            "id": n.path,
            "node_type": "note",
            "label": n.title,
            "file_path": n.path,
            "url": null,
            "link_count": 0,
        }))
        .collect();

    // Edges: all wiki-links in the vault (source_path → target note)
    let mut lresp = state
        .db
        .query("SELECT source_path, target_title FROM links WHERE vault_id = $vid")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| {
            tracing::error!("get_graph links query failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    #[derive(serde::Deserialize)]
    struct LinkRow { source_path: String, target_title: String }
    let links: Vec<LinkRow> = lresp.take(0).map_err(|e| {
        tracing::error!("get_graph links take failed: {}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    // Build title→path map for resolving target
    let title_to_path: std::collections::HashMap<String, String> = notes
        .iter()
        .map(|n| (n.title.to_lowercase(), n.path.clone()))
        .collect();

    let edges: Vec<Value> = links
        .iter()
        .filter_map(|l| {
            let target_path = title_to_path.get(&l.target_title.to_lowercase())?;
            Some(json!({
                "source_id": l.source_path,
                "target_id": target_path,
                "edge_type": "wiki_link",
                "weight": 1.0,
            }))
        })
        .collect();

    Ok(Json(json!({ "nodes": nodes, "edges": edges })))
}

// ── Graph Related ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GraphRelatedQuery {
    path: String,
    depth: Option<u32>,
    limit: Option<usize>,
}

async fn get_graph_related(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<GraphRelatedQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let depth = q.depth.unwrap_or(1).min(2);
    let limit = q.limit.unwrap_or(10).min(30);
    let start_path = q.path;

    // Load all notes → title / path for resolution
    #[derive(serde::Deserialize)]
    struct NoteRow { path: String, title: String }
    let mut resp = state
        .db
        .query("SELECT path, title FROM notes WHERE vault_id = $vid")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let note_rows: Vec<NoteRow> = resp.take(0).unwrap_or_default();

    let title_to_path: std::collections::HashMap<String, String> = note_rows
        .iter()
        .map(|n| (n.title.to_lowercase(), n.path.clone()))
        .collect();
    let path_to_title: std::collections::HashMap<String, String> = note_rows
        .iter()
        .map(|n| (n.path.clone(), n.title.clone()))
        .collect();

    // Load all links for the vault
    #[derive(serde::Deserialize)]
    struct LinkRow { source_path: String, target_title: String }
    let mut lresp = state
        .db
        .query("SELECT source_path, target_title FROM links WHERE vault_id = $vid")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let link_rows: Vec<LinkRow> = lresp.take(0).unwrap_or_default();

    // Load graph_edges (includes fact_source, wiki_link edges)
    #[derive(serde::Deserialize)]
    struct EdgeRow { source_id: String, target_id: String, edge_type: String }
    let node_prefix = format!("{}:", vault_id);
    let eresp = state
        .db
        .query("SELECT source_id, target_id, edge_type FROM graph_edges WHERE vault_id = $vid")
        .bind(("vid", vault_id.clone()))
        .await;
    let edge_rows: Vec<EdgeRow> = eresp.ok()
        .and_then(|mut r| r.take::<Vec<EdgeRow>>(0).ok())
        .unwrap_or_default();

    // Build adjacency: node_key → set of (neighbour_key, relation)
    // node_key for notes = rel_path; for memory_facts = "memory:vault_id:fact_id"
    let mut adj: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();

    // wiki-links via links table (path-based)
    for l in &link_rows {
        if let Some(target_path) = title_to_path.get(&l.target_title.to_lowercase()) {
            adj.entry(l.source_path.clone())
                .or_default()
                .push((target_path.clone(), "outgoing".to_string()));
            adj.entry(target_path.clone())
                .or_default()
                .push((l.source_path.clone(), "backlink".to_string()));
        }
    }

    // graph_edges (fact_source and others): node_ids use "vault_id:rel_path" or "memory:vault_id:fact_id"
    // Normalize: strip "vault_id:" prefix to get rel_path for note nodes
    let strip_prefix = |s: &str| -> String {
        if s.starts_with(&node_prefix) {
            s[node_prefix.len()..].to_string()
        } else {
            s.to_string()
        }
    };
    for e in &edge_rows {
        let src_key = strip_prefix(&e.source_id);
        let tgt_key = strip_prefix(&e.target_id);
        let rel = e.edge_type.clone();
        adj.entry(src_key.clone())
            .or_default()
            .push((tgt_key.clone(), rel.clone()));
        adj.entry(tgt_key)
            .or_default()
            .push((src_key, format!("←{}", rel)));
    }

    // BFS up to `depth` hops from start_path
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    visited.insert(start_path.clone());
    let mut frontier: Vec<String> = vec![start_path.clone()];
    let mut result_nodes: Vec<serde_json::Value> = Vec::new();
    let mut result_relations: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for _ in 0..depth {
        let mut next_frontier: Vec<String> = Vec::new();
        for node in &frontier {
            if let Some(neighbors) = adj.get(node) {
                for (nbr, rel) in neighbors {
                    if !visited.contains(nbr) {
                        visited.insert(nbr.clone());
                        next_frontier.push(nbr.clone());
                        result_relations.insert(nbr.clone(), rel.clone());
                    }
                }
            }
        }
        frontier = next_frontier;
        if frontier.is_empty() { break; }
    }

    // Build result (excluding start node)
    for node_key in visited.iter().filter(|p| *p != &start_path).take(limit) {
        // node_key is either a rel_path (note) or "memory:vault_id:fact_id" (memory fact)
        let is_memory = node_key.starts_with("memory:");
        let label = if is_memory {
            // Last segment is fact_id; label not directly available here
            format!("[記憶] {}", &node_key[node_key.rfind(':').map(|i| i + 1).unwrap_or(0)..])
        } else {
            path_to_title.get(node_key).cloned().unwrap_or_else(|| node_key.clone())
        };
        let rel = result_relations.get(node_key).cloned().unwrap_or_else(|| "link".to_string());
        let node_type = if is_memory { "memory_fact" } else { "note" };
        result_nodes.push(json!({
            "file_path": if is_memory { "" } else { node_key.as_str() },
            "node_id": node_key,
            "node_type": node_type,
            "label": label,
            "relation": rel,
        }));
    }

    Ok(Json(json!({ "nodes": result_nodes })))
}

// ── Backlinks ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BacklinksQuery {
    title: Option<String>,
    path: Option<String>,
}

async fn get_backlinks(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<BacklinksQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let target_title = q.title.unwrap_or_default();
    let target_path = q.path.unwrap_or_default();

    if target_title.is_empty() && target_path.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Missing title or path query param".to_string()));
    }

    // Find links where target_title matches (case-insensitive) OR target_path matches source_path
    let qb = if !target_title.is_empty() {
        state
            .db
            .query(
                "SELECT *, record::id(id) AS id FROM links WHERE vault_id = $vid \
                 AND string::lowercase(target_title) = string::lowercase($title)",
            )
            .bind(("vid", vault_id))
            .bind(("title", target_title))
    } else {
        // Find notes that link to the given path (file name without extension as title)
        let fname = std::path::Path::new(&target_path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or(target_path.clone());
        state
            .db
            .query(
                "SELECT *, record::id(id) AS id FROM links WHERE vault_id = $vid \
                 AND string::lowercase(target_title) = string::lowercase($title)",
            )
            .bind(("vid", vault_id))
            .bind(("title", fname)            )
    };

    let mut resp = qb
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp.take(0).unwrap_or_default();
    Ok(Json(json!(rows)))
}

// ── Stats ─────────────────────────────────────────────────────────────────────

async fn get_vault_stats(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    #[derive(serde::Deserialize)]
    struct CountRow { count: i64 }

    let mut r1 = state.db
        .query("SELECT count() AS count FROM notes WHERE vault_id = $vid GROUP ALL")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let total_notes: i64 = r1.take::<Vec<CountRow>>(0).unwrap_or_default()
        .into_iter().next().map(|r| r.count).unwrap_or(0);

    let mut r2 = state.db
        .query("SELECT count() AS count FROM chunks WHERE vault_id = $vid GROUP ALL")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let total_chunks: i64 = r2.take::<Vec<CountRow>>(0).unwrap_or_default()
        .into_iter().next().map(|r| r.count).unwrap_or(0);

    let mut r3 = state.db
        .query("SELECT count() AS count FROM chunks WHERE vault_id = $vid AND embedding IS NOT NONE GROUP ALL")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let embedded_chunks: i64 = r3.take::<Vec<CountRow>>(0).unwrap_or_default()
        .into_iter().next().map(|r| r.count).unwrap_or(0);

    Ok(Json(json!({
        "total": total_notes,
        "chunked": total_chunks,
        "embedded": embedded_chunks,
    })))
}

pub async fn get_vault_path(
    state: &ApiState,
    vault_id: &str,
) -> Result<String, (StatusCode, String)> {
    #[derive(serde::Deserialize)]
    struct Row {
        path: String,
    }

    let vault_id_owned = vault_id.to_string();
    let mut resp = state
        .db
        .query("SELECT path FROM vaults WHERE vault_id = $vid LIMIT 1")
        .bind(("vid", vault_id_owned))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Row> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    rows.into_iter()
        .next()
        .map(|r| r.path)
        .ok_or((StatusCode::NOT_FOUND, format!("Vault '{}' not found", vault_id)))
}

fn build_tree(path: &FsPath, root: &FsPath) -> Value {
    if path.is_dir() {
        let mut children = Vec::new();
        if let Ok(entries) = std::fs::read_dir(path) {
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| {
                let is_dir = e.path().is_dir();
                let name = e.file_name().to_string_lossy().to_lowercase();
                (!is_dir, name)
            });
            for entry in entries {
                let p = entry.path();
                let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                // Skip hidden files/dirs
                if name.starts_with('.') {
                    continue;
                }
                children.push(build_tree(&p, root));
            }
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| rel.clone());
        json!({
            "type": "folder",
            "name": name,
            "path": rel,
            "children": children,
        })
    } else {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| rel.clone());
        json!({
            "type": "file",
            "name": name,
            "path": rel,
        })
    }
}

fn collect_md_files(root: &FsPath) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    collect_md_recursive(root, &mut files);
    files
}

fn collect_md_recursive(dir: &FsPath, files: &mut Vec<std::path::PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_md_recursive(&path, files);
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            files.push(path);
        }
    }
}

fn extract_title(content: &str, fallback_path: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("# ") {
            return heading.trim().to_string();
        }
        if !trimmed.is_empty() {
            break;
        }
    }
    // Fallback: filename without extension
    FsPath::new(fallback_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| fallback_path.to_string())
}
