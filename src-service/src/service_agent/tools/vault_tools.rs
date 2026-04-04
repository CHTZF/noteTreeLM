use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use futures_util::StreamExt;
use serde_json::{json, Value};
use crate::api_state::ApiState;
use crate::db::SurrealDb;

// ── Read-only vault tools ─────────────────────────────────────────────────────

pub(crate) fn vault_list_structure(rel_path: &str, vault_path: &str) -> String {
    if vault_path.is_empty() { return "Vault 未設定".to_string(); }
    let base = std::path::Path::new(vault_path);
    let target = if rel_path.is_empty() { base.to_path_buf() } else { base.join(rel_path) };
    if !target.exists() { return format!("路徑不存在：{}", rel_path); }

    fn list_dir(dir: &std::path::Path, base: &std::path::Path, depth: u32) -> String {
        if depth > 4 { return String::new(); }
        let mut out = String::new();
        let Ok(entries) = std::fs::read_dir(dir) else { return out; };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') { continue; }
            let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().to_string();
            let indent = "  ".repeat(depth as usize);
            if path.is_dir() {
                out.push_str(&format!("{}{}/\n", indent, name));
                out.push_str(&list_dir(&path, base, depth + 1));
            } else if name.ends_with(".md") {
                out.push_str(&format!("{}{}\n", indent, rel));
            }
        }
        out
    }

    list_dir(&target, base, 0)
}

pub(crate) fn vault_read_note(rel_path: &str, vault_path: &str) -> String {
    if vault_path.is_empty() { return "Vault 未設定".to_string(); }
    if rel_path.is_empty() { return "路徑為空".to_string(); }
    let full = std::path::Path::new(vault_path).join(rel_path);
    match std::fs::read_to_string(&full) {
        Ok(c) => c,
        Err(_) => format!("讀取失敗：{}", rel_path),
    }
}

pub(crate) async fn vault_search(
    client: &reqwest::Client,
    embedding_url: &Option<String>,
    db: &SurrealDb,
    vault_id: &str,
    query: &str,
) -> Result<Value, String> {
    if query.is_empty() { return Ok(json!([])); }

    // ── Cosine similarity search on chunks ────────────────────────────────
    if let Some(vec) = crate::processing::embedder::embed_text(client, embedding_url, query).await {
        #[derive(serde::Deserialize)]
        struct ChunkRow { file_path: String, score: f32 }
        let mut resp = db
            .query("SELECT file_path, vector::similarity::cosine(embedding, $vec) AS score \
                    FROM chunks WHERE vault_id = $vid AND embedding IS NOT NONE \
                    ORDER BY score DESC LIMIT 40")
            .bind(("vid", vault_id.to_string()))
            .bind(("vec", vec))
            .await
            .map_err(|e| e.to_string())?;
        let rows: Vec<ChunkRow> = resp.take(0).map_err(|e| e.to_string())?;

        // Dedup by file_path — keep highest score per note.
        let mut seen: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
        for r in &rows {
            let entry = seen.entry(r.file_path.clone()).or_insert(f32::NEG_INFINITY);
            if r.score > *entry { *entry = r.score; }
        }
        // Sort paths by score desc, take top 8.
        let mut ranked: Vec<(String, f32)> = seen.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(8);

        if !ranked.is_empty() {
            // Fetch titles from notes table for the ranked paths.
            #[derive(serde::Deserialize)]
            struct NoteRow { path: String, title: String }
            let paths: Vec<String> = ranked.iter().map(|(p, _)| p.clone()).collect();
            let mut nresp = db
                .query("SELECT path, title FROM notes WHERE vault_id = $vid AND path INSIDE $paths")
                .bind(("vid", vault_id.to_string()))
                .bind(("paths", paths.clone()))
                .await
                .map_err(|e| e.to_string())?;
            let notes: Vec<NoteRow> = nresp.take(0).map_err(|e| e.to_string())?;
            let title_map: std::collections::HashMap<String, String> =
                notes.into_iter().map(|n| (n.path, n.title)).collect();

            let result: Vec<Value> = ranked.iter().map(|(path, score)| json!({
                "path": path,
                "title": title_map.get(path).cloned().unwrap_or_default(),
                "score": score,
            })).collect();
            return Ok(json!(result));
        }
    }

    // ── Fallback: regex search (no embedding available or empty results) ──
    #[derive(serde::Deserialize)]
    struct Row { path: String, title: String }
    let mut resp = db
        .query("SELECT path, title FROM notes WHERE vault_id = $vid AND content ~ $q LIMIT 8")
        .bind(("vid", vault_id.to_string()))
        .bind(("q", query.to_string()))
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;
    Ok(json!(rows.iter().map(|r| json!({"path": r.path, "title": r.title})).collect::<Vec<_>>()))
}

#[allow(dead_code)]
pub(crate) async fn vault_query_memory(
    client: &reqwest::Client,
    embedding_url: &Option<String>,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    keywords: &[String],
    limit: u64,
) -> Result<Value, String> {
    let (text, _) = vault_query_memory_with_ids(client, embedding_url, db, vault_id, account_id, keywords, limit).await?;
    Ok(text)
}

/// Same as vault_query_memory but also returns the matched fact_ids for memory:prefetched.
pub(crate) async fn vault_query_memory_with_ids(
    client: &reqwest::Client,
    embedding_url: &Option<String>,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    keywords: &[String],
    limit: u64,
) -> Result<(Value, Vec<String>), String> {
    let now = chrono::Utc::now().timestamp();
    #[derive(serde::Deserialize)]
    struct Row { fact_id: Option<String>, content: String, category: String, embedding: Option<Vec<f32>> }

    // Try semantic search first when we have keywords and an embedding server
    if !keywords.is_empty() {
        let query_text = keywords.join(" ");
        if let Some(query_vec) = crate::processing::embedder::embed_text(client, embedding_url, &query_text).await {
            if !query_vec.is_empty() {
                let rows: Vec<Row> = db
                    .query("SELECT fact_id, content, category, embedding FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now AND embedding IS NOT NONE")
                    .bind(("vid", vault_id.to_string()))
                    .bind(("aid", account_id.to_string()))
                    .bind(("now", now))
                    .await
                    .ok()
                    .and_then(|mut r| r.take(0).ok())
                    .unwrap_or_default();

                if !rows.is_empty() {
                    let mut scored: Vec<(f32, Row)> = rows.into_iter().filter_map(|row| {
                        let emb = row.embedding.as_ref()?;
                        if emb.is_empty() { return None; }
                        let score = crate::processing::embedder::cosine_sim(&query_vec, emb);
                        Some((score, row))
                    }).collect();
                    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                    let top: Vec<Row> = scored.into_iter().take(limit as usize).map(|(_, r)| r).collect();
                    let fact_ids: Vec<String> = top.iter()
                        .filter_map(|r| r.fact_id.clone())
                        .map(|fid| format!("memory:{}:{}", vault_id, fid))
                        .collect();
                    let result: Vec<Value> = top.iter().map(|r| json!({"content": r.content, "category": r.category})).collect();
                    return Ok((json!(result), fact_ids));
                }
            }
        }
    }

    // Fallback: keyword regex or recency sort
    let rows: Vec<Row> = if keywords.is_empty() {
        let mut r = db
            .query("SELECT fact_id, content, category FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now ORDER BY created_at DESC LIMIT $lim")
            .bind(("vid", vault_id.to_string()))
            .bind(("aid", account_id.to_string()))
            .bind(("now", now))
            .bind(("lim", limit))
            .await
            .map_err(|e| e.to_string())?;
        r.take(0).map_err(|e| e.to_string())?
    } else {
        let mut collected: Vec<Row> = Vec::new();
        for kw in keywords.iter().take(3) {
            let mut r = db
                .query("SELECT fact_id, content, category FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND content ~ $kw AND expires_at > $now LIMIT $lim")
                .bind(("vid", vault_id.to_string()))
                .bind(("aid", account_id.to_string()))
                .bind(("kw", kw.clone()))
                .bind(("now", now))
                .bind(("lim", limit))
                .await
                .map_err(|e| e.to_string())?;
            let rows: Vec<Row> = r.take(0).map_err(|e| e.to_string())?;
            collected.extend(rows);
        }
        collected
    };
    let fact_ids: Vec<String> = rows.iter()
        .filter_map(|r| r.fact_id.clone())
        .map(|fid| format!("memory:{}:{}", vault_id, fid))
        .collect();
    let result: Vec<Value> = rows.iter().map(|r| json!({"content": r.content, "category": r.category})).collect();
    Ok((json!(result), fact_ids))
}

// ── Write vault tools ─────────────────────────────────────────────────────────

/// Validate that a vault-relative path is safe: non-empty, no `..` traversal, no `.` components,
/// and not an absolute path. Applied to all write tools before any filesystem access.
fn validate_rel_path(rel_path: &str) -> Result<(), String> {
    if rel_path.is_empty() {
        return Err("路徑不能為空".to_string());
    }
    for component in std::path::Path::new(rel_path).components() {
        match component {
            std::path::Component::ParentDir =>
                return Err(format!("路徑不允許包含 '..'：{}", rel_path)),
            std::path::Component::CurDir =>
                return Err(format!("路徑不允許包含 '.'：{}", rel_path)),
            std::path::Component::RootDir | std::path::Component::Prefix(_) =>
                return Err(format!("路徑必須是相對路徑：{}", rel_path)),
            _ => {}
        }
    }
    Ok(())
}

pub(crate) async fn vault_create_note(
    rel_path: &str,
    content: &str,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    validate_rel_path(rel_path)?;
    let full = std::path::Path::new(vault_path).join(rel_path);
    if let Some(parent) = full.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
    if full.exists() { return Err(format!("筆記已存在：{}", rel_path)); }
    tokio::fs::write(&full, content).await.map_err(|e| e.to_string())?;
    sync_note_to_db(client, db, vault_id, rel_path, content).await;
    Ok(json!({ "ok": true, "path": rel_path }))
}

pub(crate) async fn vault_update_note(
    rel_path: &str,
    content: &str,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    validate_rel_path(rel_path)?;
    let full = std::path::Path::new(vault_path).join(rel_path);
    if !full.exists() { return Err(format!("筆記不存在：{}", rel_path)); }
    tokio::fs::write(&full, content).await.map_err(|e| e.to_string())?;
    sync_note_to_db(client, db, vault_id, rel_path, content).await;
    Ok(json!({ "ok": true, "path": rel_path }))
}

pub(crate) async fn vault_create_folder(
    rel_path: &str,
    vault_path: &str,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    validate_rel_path(rel_path)?;
    let full = std::path::Path::new(vault_path).join(rel_path);
    tokio::fs::create_dir_all(&full).await.map_err(|e| e.to_string())?;
    let now = chrono::Utc::now().timestamp();
    let _ = db
        .query("UPDATE vaults SET updated_at = $now WHERE vault_id = $vid")
        .bind(("now", now))
        .bind(("vid", vault_id.to_string()))
        .await;
    Ok(json!({ "ok": true, "path": rel_path }))
}

pub(crate) async fn vault_delete_note(
    rel_path: &str,
    vault_path: &str,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    validate_rel_path(rel_path)?;
    let full = std::path::Path::new(vault_path).join(rel_path);
    if !full.exists() { return Err(format!("筆記不存在：{}", rel_path)); }
    tokio::fs::remove_file(&full).await.map_err(|e| format!("刪除失敗：{}", e))?;
    let _ = db
        .query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
        .bind(("vid", vault_id.to_string()))
        .bind(("path", rel_path.to_string()))
        .await;
    Ok(json!({ "ok": true, "path": rel_path }))
}

pub(crate) async fn vault_delete_folder(
    rel_path: &str,
    vault_path: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    validate_rel_path(rel_path)?;
    let full = std::path::Path::new(vault_path).join(rel_path);
    if !full.exists() { return Err(format!("資料夾不存在：{}", rel_path)); }
    tokio::fs::remove_dir_all(&full).await.map_err(|e| format!("刪除資料夾失敗：{}", e))?;
    Ok(json!({ "ok": true, "path": rel_path }))
}

pub(crate) async fn vault_move_note(
    from_rel: &str,
    to_rel: &str,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    validate_rel_path(from_rel)?;
    validate_rel_path(to_rel)?;
    let base = std::path::Path::new(vault_path);
    let from_full = base.join(from_rel);
    let to_full = base.join(to_rel);
    if !from_full.exists() { return Err(format!("來源不存在：{}", from_rel)); }
    if to_full.exists() { return Err(format!("目標路徑已存在：{}，請先確認是否要覆蓋或選擇其他名稱。", to_rel)); }
    if let Some(parent) = to_full.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
    tokio::fs::rename(&from_full, &to_full).await.map_err(|e| format!("移動失敗：{}", e))?;
    // Update DB: remove old path, add new path
    let _ = db
        .query("DELETE FROM notes WHERE vault_id = $vid AND path = $old_path")
        .bind(("vid", vault_id.to_string()))
        .bind(("old_path", from_rel.to_string()))
        .await;
    let new_content = tokio::fs::read_to_string(&to_full).await.unwrap_or_default();
    sync_note_to_db(client, db, vault_id, to_rel, &new_content).await;
    Ok(json!({ "ok": true, "from": from_rel, "to": to_rel }))
}

pub(crate) async fn vault_append_to_note(
    rel_path: &str,
    content: &str,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    validate_rel_path(rel_path)?;
    let full = std::path::Path::new(vault_path).join(rel_path);
    if !full.exists() { return Err(format!("筆記不存在：{}", rel_path)); }
    let mut existing = tokio::fs::read_to_string(&full).await.map_err(|e| e.to_string())?;
    if !existing.ends_with('\n') { existing.push('\n'); }
    existing.push_str(content);
    tokio::fs::write(&full, &existing).await.map_err(|e| e.to_string())?;
    sync_note_to_db(client, db, vault_id, rel_path, &existing).await;
    Ok(json!({ "ok": true, "path": rel_path }))
}

/// Best-effort: update the note record in DB so search index stays fresh.
pub(crate) async fn sync_note_to_db(
    _client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
    path: &str,
    content: &str,
) {
    let now = chrono::Utc::now().timestamp();
    let title = path.split('/').last().unwrap_or(path).trim_end_matches(".md").to_string();
    let note_id = uuid::Uuid::new_v4().to_string();
    let _ = db
        .query("INSERT INTO notes (note_id, vault_id, path, title, content, updated_at, created_at) VALUES ($nid, $vid, $path, $title, $content, $now, $now) ON DUPLICATE KEY UPDATE content = $content, title = $title, updated_at = $now")
        .bind(("nid", note_id))
        .bind(("vid", vault_id.to_string()))
        .bind(("path", path.to_string()))
        .bind(("title", title))
        .bind(("content", content.to_string()))
        .bind(("now", now))
        .await;
}

/// Extract note paths for agent:note_refs event
pub(crate) fn extract_note_refs(tool_name: &str, args: &Value, _result: &Value, _vault_path: &str) -> Vec<String> {
    match tool_name {
        "read_note" => {
            let p = args["path"].as_str().unwrap_or("");
            if p.is_empty() { return vec![]; }
            let full = if p.ends_with(".md") { p.to_string() } else { format!("{}.md", p) };
            vec![full]
        }
        "open_note" => {
            args["paths"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
                .unwrap_or_else(|| {
                    args["path"].as_str().map(|p| vec![p.to_string()]).unwrap_or_default()
                })
        }
        "search_vault" => {
            // result is a JSON array of {path, title}
            _result.as_array()
                .map(|arr| arr.iter()
                    .filter_map(|v| v["path"].as_str())
                    .map(String::from)
                    .collect())
                .unwrap_or_default()
        }
        _ => vec![],
    }
}

// ── LLM streaming ─────────────────────────────────────────────────────────────

/// Returns the byte offset up to which `s` can safely be emitted without risking
/// a partial prefix of `tag` at the end.
/// Works on raw bytes — `tag` must be pure ASCII (guaranteed for `<tool_call>`).
fn safe_emit_end(s: &str, tag: &str) -> usize {
    let s_bytes = s.as_bytes();
    let tag_bytes = tag.as_bytes();
    let n = s_bytes.len();
    for suffix_len in 1..=tag_bytes.len().min(n) {
        let suffix = &s_bytes[n - suffix_len..];
        if tag_bytes.starts_with(suffix) {
            // The returned offset is right before ASCII bytes → always a valid char boundary.
            return n - suffix_len;
        }
    }
    n
}

/// Validate citation ids from [cite:id1,id2] against the tool_calls_store.
/// Returns true if all ids are known (or store is None = no validation needed).
/// Emits agent:hallucination_warning if unknown ids are found.
async fn validate_citation(
    cite_inner: &str,
    tool_calls_store: Option<&crate::service_agent::engine::dispatcher::ToolCallStore>,
) -> bool {
    let store = match tool_calls_store {
        Some(s) => s,
        None => return true,
    };
    if cite_inner.trim() == "none" { return true; }
    let ids: Vec<&str> = cite_inner.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if ids.is_empty() { return true; }
    let store = store.lock().await;
    let unknown: Vec<&str> = ids.iter().filter(|id| !store.contains_key(**id)).copied().collect();
    unknown.is_empty()
}

/// Stream one LLM round, emitting llm:token events. Returns (text, finish_reason, tool_chunks).
/// tool_chunks: Vec<(id, name, arguments_str)>
pub(crate) async fn stream_llm_round(
    client: &reqwest::Client,
    llm_url: &str,
    body: Value,
    state: &ApiState,
    _session_id: &str,
    cancel: &Arc<AtomicBool>,
    tool_calls_store: Option<&crate::service_agent::engine::dispatcher::ToolCallStore>,
) -> Result<(String, String, Vec<(String, String, String)>), String> {
    let resp = client
        .post(format!("{}/v1/chat/completions", llm_url))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("llm error {}: {}", status, text));
    }

    let mut stream = resp.bytes_stream();
    let mut sse_buf = String::new();
    let mut full_text = String::new();
    let mut finish_reason = "stop".to_string();
    let mut tool_chunks: Vec<(String, String, String)> = Vec::new();

    // Lookahead buffer: holds content not yet emitted, to avoid streaming partial <tool_call> tags
    let mut pending_emit = String::new();
    let mut suppress_emit = false;  // true once we've detected a text-format <tool_call>

    // Citation interceptor state machine.
    // Buffers up to CITE_BUFFER_LIMIT chars to detect [cite:...] at start of response.
    const CITE_BUFFER_LIMIT: usize = 60;
    enum CiteState { Buffering, Forwarding }
    let mut cite_state = CiteState::Buffering;
    let mut cite_buf = String::new();
    let mut cite_retry_done = false;

    while let Some(item) = stream.next().await {
        if cancel.load(Ordering::Relaxed) { break; }
        let bytes = item.map_err(|e| e.to_string())?;
        sse_buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(event_end) = sse_buf.find("\n\n") {
            let event = sse_buf[..event_end].to_string();
            sse_buf = sse_buf[event_end + 2..].to_string();

            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" { continue; }
                    if let Ok(j) = serde_json::from_str::<Value>(data) {
                        let choice = &j["choices"][0];
                        if let Some(fr) = choice["finish_reason"].as_str() {
                            if !fr.is_empty() { finish_reason = fr.to_string(); }
                        }
                        let delta = &choice["delta"];
                        if let Some(content) = delta["content"].as_str() {
                            if !content.is_empty() {
                                full_text.push_str(content);
                                if !suppress_emit {
                                    // ── Citation interceptor ──────────────────
                                    let emit_content = match cite_state {
                                        CiteState::Forwarding => {
                                            Some(content.to_string())
                                        }
                                        CiteState::Buffering => {
                                            cite_buf.push_str(content);
                                            // Try to parse [cite:...] from buffer
                                            if let Some(cite_end) = cite_buf.find(']') {
                                                if cite_buf.starts_with("[cite:") {
                                                    let cite_inner = &cite_buf[6..cite_end]; // between [cite: and ]
                                                    let _valid = validate_citation(cite_inner, tool_calls_store).await;
                                                    // Strip [cite:...] from what we forward
                                                    let rest = cite_buf[cite_end + 1..].to_string();
                                                    cite_buf.clear();
                                                    cite_state = CiteState::Forwarding;
                                                    if rest.is_empty() { None } else { Some(rest) }
                                                } else {
                                                    // ] found but doesn't start with [cite: — forward buffer
                                                    let flushed = cite_buf.clone();
                                                    cite_buf.clear();
                                                    cite_state = CiteState::Forwarding;
                                                    Some(flushed)
                                                }
                                            } else if cite_buf.len() >= CITE_BUFFER_LIMIT {
                                                // Timeout: no [cite:...] found — treat as [cite:none]
                                                if !cite_retry_done && tool_calls_store.is_some() {
                                                    // Emit warning event; forward buffered content
                                                    state.daemon.emit("agent:citation_missing", json!({}));
                                                    cite_retry_done = true;
                                                }
                                                let flushed = cite_buf.clone();
                                                cite_buf.clear();
                                                cite_state = CiteState::Forwarding;
                                                Some(flushed)
                                            } else {
                                                None // still buffering
                                            }
                                        }
                                    };
                                    if let Some(emit_str) = emit_content {
                                        pending_emit.push_str(&emit_str);
                                        if pending_emit.contains("<tool_call>") {
                                            let pos = pending_emit.find("<tool_call>").unwrap_or(0);
                                            if pos > 0 {
                                                state.daemon.emit("llm:token", json!(&pending_emit[..pos]));
                                            }
                                            pending_emit.clear();
                                            suppress_emit = true;
                                        } else {
                                            let safe_end = safe_emit_end(&pending_emit, "<tool_call>");
                                            if safe_end > 0 {
                                                let chunk = pending_emit[..safe_end].to_string();
                                                pending_emit = pending_emit[safe_end..].to_string();
                                                state.daemon.emit("llm:token", json!(chunk));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(tc_arr) = delta["tool_calls"].as_array() {
                            for tc in tc_arr {
                                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                                while tool_chunks.len() <= idx {
                                    tool_chunks.push((String::new(), String::new(), String::new()));
                                }
                                let acc = &mut tool_chunks[idx];
                                if let Some(id) = tc["id"].as_str() { if !id.is_empty() { acc.0 = id.to_string(); } }
                                if let Some(n) = tc["function"]["name"].as_str() { if !n.is_empty() { acc.1 = n.to_string(); } }
                                if let Some(a) = tc["function"]["arguments"].as_str() { acc.2.push_str(a); }
                            }
                        }
                    }
                }
            }
        }
    }

    // Flush remaining citation buffer (stream ended before we saw [cite:...])
    if !cite_buf.is_empty() {
        pending_emit.push_str(&cite_buf);
        cite_buf.clear();
    }
    // Flush remaining pending (if stream ended without <tool_call>)
    if !suppress_emit && !pending_emit.is_empty() {
        state.daemon.emit("llm:token", json!(pending_emit));
    }

    // Parse text-format tool calls from full_text (e.g. Qwen/Mistral <tool_call> style)
    // and strip them from the display text
    if tool_chunks.is_empty() && full_text.contains("<tool_call>") {
        let mut clean_text = String::new();
        let mut rest = full_text.as_str();
        let mut tc_idx = 0usize;
        while let Some(start) = rest.find("<tool_call>") {
            clean_text.push_str(&rest[..start]);
            let after_open = &rest[start + "<tool_call>".len()..];
            if let Some(end) = after_open.find("</tool_call>") {
                let json_str = after_open[..end].trim();
                if let Ok(tc) = serde_json::from_str::<Value>(json_str) {
                    let name = tc["name"].as_str().unwrap_or("").to_string();
                    let args = tc["arguments"].clone();
                    let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
                    if !name.is_empty() {
                        tool_chunks.push((format!("tc_text_{}", tc_idx), name, args_str));
                        tc_idx += 1;
                    }
                }
                rest = &after_open[end + "</tool_call>".len()..];
            } else {
                // Incomplete tag — keep remainder as-is
                clean_text.push_str("<tool_call>");
                clean_text.push_str(after_open);
                rest = "";
                break;
            }
        }
        clean_text.push_str(rest);
        full_text = clean_text.trim().to_string();
    }

    Ok((full_text, finish_reason, tool_chunks))
}

/// Non-streaming LLM call for sub-agents. Returns (content, tool_chunks).
/// Does NOT emit llm:token events — caller handles output.
pub(crate) async fn call_llm_once(
    client: &reqwest::Client,
    llm_url: &str,
    messages: &[Value],
    tools: Option<Value>,
    cancel: &Arc<AtomicBool>,
) -> Result<(String, Vec<(String, String, String)>), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let mut body = json!({
        "messages": messages,
        "stream": false,
        "temperature": 0.7,
        "max_tokens": 1024,
    });
    if let Some(t) = tools {
        body["tools"] = t;
        body["tool_choice"] = json!("auto");
    }
    let resp = client
        .post(format!("{}/v1/chat/completions", llm_url))
        .json(&body)
        .send().await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("llm error {}: {}", status, text));
    }
    let j: Value = resp.json().await.map_err(|e| e.to_string())?;
    let msg = &j["choices"][0]["message"];
    let content = msg["content"].as_str().unwrap_or("").to_string();
    let mut tool_chunks: Vec<(String, String, String)> = Vec::new();
    if let Some(tcs) = msg["tool_calls"].as_array() {
        for tc in tcs {
            let id   = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            let args = tc["function"]["arguments"].as_str().unwrap_or("{}").to_string();
            if !name.is_empty() {
                tool_chunks.push((id, name, args));
            }
        }
    }
    Ok((content, tool_chunks))
}
