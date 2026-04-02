use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use futures_util::StreamExt;
use serde_json::{json, Value};
use crate::api_state::ApiState;
use crate::db::SurrealDb;

// ── Rollback infrastructure ───────────────────────────────────────────────────

/// Tracks how to undo a completed write operation (carried in DispatchResult).
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum RollbackEntry {
    WriteFile  { path: String, content: String },
    RestoreFile { path: String, content: String },
    DeleteFile { path: String },
    RemoveDir  { path: String },
    MoveFile   { from_abs: String, to_abs: String },
}


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

pub(crate) async fn vault_create_note(
    rel_path: &str,
    content: &str,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    let full = std::path::Path::new(vault_path).join(rel_path);
    if let Some(parent) = full.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
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
    let base = std::path::Path::new(vault_path);
    let from_full = base.join(from_rel);
    let to_full = base.join(to_rel);
    if !from_full.exists() { return Err(format!("來源不存在：{}", from_rel)); }
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

// ── Tool classification helpers ───────────────────────────────────────────────

/// Write tool check for interactive agent: tools that modify vault state and require user confirmation.
pub(crate) fn is_interactive_write_tool(name: &str) -> bool {
    matches!(name,
        "create_note" | "update_note" | "create_folder" |
        "delete_note" | "delete_folder" | "move_note" | "append_to_note" |
        "create_agent_skill"
    )
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

// ── Tool dispatcher ───────────────────────────────────────────────────────────

/// Dispatch result: value + optional rollback entry (for write operations).
pub(crate) type DispatchResult = Result<(Value, Option<RollbackEntry>), String>;

/// Tool dispatcher for interactive agent (vault tools + agent tools + memory tools).
/// Returns (result_value, optional_rollback_entry).
pub(crate) async fn dispatch_interactive_tool(
    client: &reqwest::Client,
    llm_url: &str,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    vault_path: &str,
    embedding_url: &Option<String>,
    name: &str,
    args: &Value,
) -> DispatchResult {
    match name {
        // ── Special tools (SSE side-effects emitted by the calling closure) ──
        "plan_announce" => {
            // Executor has already emitted agent:plan_announce via the tool closure
            Ok((json!("✅ 已確認計畫，請立即執行"), None))
        }
        "open_note" => {
            // Executor closure emits agent:open_note + agent:note_refs
            let paths: Vec<Value> = args["paths"].as_array()
                .cloned()
                .unwrap_or_else(|| {
                    args["path"].as_str().map(|p| vec![json!(p)]).unwrap_or_default()
                });
            Ok((json!({ "opened": paths }), None))
        }
        "think" | "live_respond" => {
            // Auto-confirm; live_respond args are consumed by the caller (runner.rs).
            Ok((json!("✅"), None))
        }

        // ── Vault read tools ─────────────────────────────────────────────────
        "list_structure" => {
            let path = args["path"].as_str().unwrap_or("");
            Ok((Value::String(vault_list_structure(path, vault_path)), None))
        }
        "read_note" => {
            let raw_path = args["path"].as_str().unwrap_or("");
            let path = if raw_path.ends_with(".md") { raw_path.to_string() } else { format!("{}.md", raw_path) };
            Ok((Value::String(vault_read_note(&path, vault_path)), None))
        }
        "search_vault" => {
            let query = args["query"].as_str().unwrap_or("");
            vault_search(client, embedding_url, db, vault_id, query).await.map(|v| (v, None))
        }
        "query_memory" => {
            let keywords: Vec<String> = args["keywords"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
                .unwrap_or_default();
            let limit = args["limit"].as_u64().unwrap_or(5).min(20);
            vault_query_memory_with_ids(client, embedding_url, db, vault_id, account_id, &keywords, limit).await.map(|(v, _)| (v, None))
        }

        // ── Vault write tools (with rollback) ────────────────────────────────
        "create_note" => {
            let path = args["path"].as_str().unwrap_or("");
            let path = if path.ends_with(".md") { path.to_string() } else { format!("{}.md", path) };
            let content = args["content"].as_str().unwrap_or("");
            let full_abs = std::path::Path::new(vault_path).join(&path).to_string_lossy().to_string();
            let file_existed = std::path::Path::new(&full_abs).exists();
            let original = if file_existed { tokio::fs::read_to_string(&full_abs).await.unwrap_or_default() } else { String::new() };
            vault_create_note(&path, content, vault_path, client, db, vault_id).await.map(|v| {
                let rollback = if file_existed {
                    RollbackEntry::WriteFile { path: full_abs, content: original }
                } else {
                    RollbackEntry::DeleteFile { path: full_abs }
                };
                (v, Some(rollback))
            })
        }
        "update_note" => {
            let path = args["path"].as_str().unwrap_or("");
            let path = if path.ends_with(".md") { path.to_string() } else { format!("{}.md", path) };
            let content = args["content"].as_str().unwrap_or("");
            let full_abs = std::path::Path::new(vault_path).join(&path).to_string_lossy().to_string();
            // Read original before overwrite (for rollback)
            let original = tokio::fs::read_to_string(&full_abs).await.unwrap_or_default();
            vault_update_note(&path, content, vault_path, client, db, vault_id).await.map(|v| {
                let rollback = RollbackEntry::WriteFile { path: full_abs, content: original };
                (v, Some(rollback))
            })
        }
        "create_folder" => {
            let path = args["path"].as_str().unwrap_or("");
            let full_abs = std::path::Path::new(vault_path).join(path).to_string_lossy().to_string();
            vault_create_folder(path, vault_path, db, vault_id).await.map(|v| {
                let rollback = RollbackEntry::RemoveDir { path: full_abs };
                (v, Some(rollback))
            })
        }
        "delete_note" => {
            let path = args["path"].as_str().unwrap_or("");
            let path = if path.ends_with(".md") { path.to_string() } else { format!("{}.md", path) };
            let full_abs = std::path::Path::new(vault_path).join(&path).to_string_lossy().to_string();
            // Read content before delete (for rollback restore)
            let original = tokio::fs::read_to_string(&full_abs).await.unwrap_or_default();
            vault_delete_note(&path, vault_path, db, vault_id).await.map(|v| {
                let rollback = RollbackEntry::RestoreFile { path: full_abs, content: original };
                (v, Some(rollback))
            })
        }
        "delete_folder" => {
            let path = args["path"].as_str().unwrap_or("");
            // Folder deletion is not fully reversible (contents lost); rollback is a no-op marker
            vault_delete_folder(path, vault_path).await.map(|v| (v, None))
        }
        "move_note" => {
            let from = args["from"].as_str().unwrap_or("");
            let to = args["to"].as_str().unwrap_or("");
            let from = if from.ends_with(".md") { from.to_string() } else { format!("{}.md", from) };
            let to = if to.ends_with(".md") { to.to_string() } else { format!("{}.md", to) };
            let from_abs = std::path::Path::new(vault_path).join(&from).to_string_lossy().to_string();
            let to_abs = std::path::Path::new(vault_path).join(&to).to_string_lossy().to_string();
            vault_move_note(&from, &to, vault_path, client, db, vault_id).await.map(|v| {
                // Rollback: move back (from_abs ← to_abs)
                let rollback = RollbackEntry::MoveFile { from_abs: to_abs, to_abs: from_abs };
                (v, Some(rollback))
            })
        }
        "append_to_note" => {
            let path = args["path"].as_str().unwrap_or("");
            let path = if path.ends_with(".md") { path.to_string() } else { format!("{}.md", path) };
            let content = args["content"].as_str().unwrap_or("");
            let full_abs = std::path::Path::new(vault_path).join(&path).to_string_lossy().to_string();
            let original = tokio::fs::read_to_string(&full_abs).await.unwrap_or_default();
            vault_append_to_note(&path, content, vault_path, client, db, vault_id).await.map(|v| {
                let rollback = RollbackEntry::WriteFile { path: full_abs, content: original };
                (v, Some(rollback))
            })
        }

        // ── Agent tools ──────────────────────────────────────────────────────
        "search_skills" => {
            let query = args["query"].as_str().unwrap_or("");
            #[derive(serde::Deserialize)]
            struct Row { title: String, behavior: String, tool_calls: Option<Value>, embedding: Option<Value> }
            let mut resp = db
                .query("SELECT title, behavior, tool_calls, embedding FROM agent_skills WHERE account_id = $aid AND is_active = true LIMIT 30")
                .bind(("aid", account_id.to_string()))
                .await
                .map_err(|e| e.to_string())?;
            let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;

            // Prefer semantic search; fall back to keyword filter
            let q_vec = if !query.is_empty() {
                crate::processing::embedder::embed_text(client, embedding_url, query).await
            } else {
                None
            };

            let results: Vec<Value> = if let Some(ref qv) = q_vec {
                // Semantic: score every skill that has an embedding, threshold 0.60
                let mut scored: Vec<(f32, &Row)> = rows.iter().filter_map(|r| {
                    let emb: Vec<f32> = r.embedding.as_ref()?.as_array()?
                        .iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
                    if emb.is_empty() { return None; }
                    let score = crate::service_agent::helpers::cosine_sim(qv, &emb);
                    if score >= 0.60 { Some((score, r)) } else { None }
                }).collect();
                scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                scored.into_iter().take(10).map(|(_, r)| json!({
                    "title": r.title,
                    "behavior": r.behavior,
                    "required_tools": r.tool_calls,
                })).collect()
            } else {
                // Keyword fallback
                let q_lower = query.to_lowercase();
                rows.iter()
                    .filter(|r| q_lower.is_empty() || r.title.to_lowercase().contains(&q_lower) || r.behavior.to_lowercase().contains(&q_lower))
                    .take(10)
                    .map(|r| json!({
                        "title": r.title,
                        "behavior": r.behavior,
                        "required_tools": r.tool_calls,
                    }))
                    .collect()
            };
            Ok((json!(results), None))
        }
        "create_agent_skill" => {
            let title = args["title"].as_str().unwrap_or("").to_string();
            let trigger = args["trigger"].as_str().unwrap_or("").to_string();
            let behavior = args["behavior"].as_str().unwrap_or("").to_string();
            let injection_mode = args["injection_mode"].as_str().unwrap_or("passive").to_string();
            let skill_id = uuid::Uuid::new_v4().to_string();
            let now = chrono::Utc::now().timestamp();
            db.query("INSERT INTO agent_skills (skill_id, account_id, title, trigger, behavior, is_active, trigger_count, injection_mode, created_at) VALUES ($sid, $aid, $title, $trigger, $behavior, true, 0, $imode, $now)")
                .bind(("sid", skill_id.clone())).bind(("aid", account_id.to_string()))
                .bind(("title", title)).bind(("trigger", trigger)).bind(("behavior", behavior))
                .bind(("imode", injection_mode)).bind(("now", now))
                .await
                .map_err(|e| e.to_string())?;
            // Trigger embedding in background so the new skill is searchable immediately.
            let db_c = db.clone();
            let aid = account_id.to_string();
            let eu = embedding_url.clone();
            tokio::spawn(async move {
                crate::db::seeds::embed_skills_for_account(&db_c, &aid, &eu).await;
            });
            Ok((json!({ "ok": true, "skill_id": skill_id }), None))
        }

        // ── Memory agent tools ───────────────────────────────────────────────
        "get_unprocessed_conversations" => {
            let limit = args["limit"].as_i64().unwrap_or(20);
            super::memory_tools::get_unprocessed_conversations(db, vault_id, account_id, limit)
                .await.map(|v| (v, None))
        }
        "get_conversation_content" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            let skip = args["skip_count"].as_i64().unwrap_or(0);
            let char_limit = args["char_limit"].as_i64().unwrap_or(500);
            super::memory_tools::get_conversation_content(db, &conv_id, skip, char_limit)
                .await.map(|v| (v, None))
        }
        "save_memory_facts" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            let facts = args["facts"].as_array().cloned().unwrap_or_default();
            super::memory_tools::save_memory_facts(client, db, vault_id, account_id, &conv_id, facts, embedding_url)
                .await.map(|v| (v, None))
        }
        "mark_conversation_processed" => {
            let conv_id = args["conversation_id"].as_str().unwrap_or("").to_string();
            super::memory_tools::mark_conversation_processed(db, &conv_id)
                .await.map(|v| (v, None))
        }
        "condense_memory_facts" => {
            let category = args["category"].as_str().map(String::from);
            super::memory_tools::condense_memory_facts(client, llm_url, db, vault_id, account_id, category, embedding_url)
                .await.map(|v| (v, None))
        }
        _ => Err(format!("unknown tool: {}", name)),
    }
}

// ── Tool schemas ──────────────────────────────────────────────────────────────

/// Build the tools schema for interactive agent.
/// Includes vault tools, agent tools, and memory tools.
/// `open_note` and `plan_announce` are always included.
pub(crate) fn build_tools_schema_interactive(tool_names: &[String]) -> Vec<Value> {
    let all_tools: Vec<Value> = vec![
        // ── Read tools ───────────────────────────────────────────────────────
        json!({ "type": "function", "function": {
            "name": "list_structure",
            "description": "列出 vault 的資料夾和筆記結構",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "子路徑，省略則顯示根目錄" }
            }, "required": [] }
        }}),
        json!({ "type": "function", "function": {
            "name": "read_note",
            "description": "讀取指定路徑的筆記內容",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "筆記的相對路徑（可省略 .md）" }
            }, "required": ["path"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "search_vault",
            "description": "在 vault 中搜尋相關筆記",
            "parameters": { "type": "object", "properties": {
                "query": { "type": "string", "description": "搜尋關鍵字" }
            }, "required": ["query"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "query_memory",
            "description": "查詢長期記憶事實",
            "parameters": { "type": "object", "properties": {
                "keywords": { "type": "array", "items": { "type": "string" }, "description": "關鍵字列表" },
                "limit": { "type": "number", "description": "最多幾條，預設 5" }
            }, "required": [] }
        }}),
        // ── Write tools ──────────────────────────────────────────────────────
        json!({ "type": "function", "function": {
            "name": "create_note",
            "description": "建立新筆記",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "筆記路徑（可省略 .md）" },
                "content": { "type": "string", "description": "筆記內容（Markdown）" }
            }, "required": ["path", "content"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "update_note",
            "description": "更新現有筆記的全部內容",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "筆記路徑（可省略 .md）" },
                "content": { "type": "string", "description": "新的筆記內容" }
            }, "required": ["path", "content"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "append_to_note",
            "description": "在現有筆記末尾追加內容",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "筆記路徑（可省略 .md）" },
                "content": { "type": "string", "description": "要追加的內容" }
            }, "required": ["path", "content"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "create_folder",
            "description": "建立新資料夾",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "資料夾相對路徑" }
            }, "required": ["path"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "delete_note",
            "description": "刪除指定的筆記（不可恢復）",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "筆記路徑（可省略 .md）" }
            }, "required": ["path"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "delete_folder",
            "description": "刪除整個資料夾及其內容（不可恢復）",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string", "description": "資料夾路徑" }
            }, "required": ["path"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "move_note",
            "description": "移動或重新命名筆記",
            "parameters": { "type": "object", "properties": {
                "from": { "type": "string", "description": "來源路徑（可省略 .md）" },
                "to":   { "type": "string", "description": "目標路徑（可省略 .md）" }
            }, "required": ["from", "to"] }
        }}),
        // ── Agent / UI tools ─────────────────────────────────────────────────
        json!({ "type": "function", "function": {
            "name": "open_note",
            "description": "在編輯器中打開指定筆記，讓使用者查看。呼叫後對話結束。",
            "parameters": { "type": "object", "properties": {
                "paths": { "type": "array", "items": { "type": "string" }, "description": "要打開的筆記路徑列表" }
            }, "required": ["paths"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "plan_announce",
            "description": "在執行寫入操作前，向使用者宣告計畫。呼叫後自動繼續執行，不需確認。",
            "parameters": { "type": "object", "properties": {
                "plan": { "type": "string", "description": "即將執行的操作計畫描述" }
            }, "required": ["plan"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "create_agent_skill",
            "description": "建立新的 agent 技能。behavior 欄位用自然語言描述，工具鏈用 @[tool_name] 標記（如 @[search_vault] 找筆記後 @[update_note] 更新）。",
            "parameters": { "type": "object", "properties": {
                "title":          { "type": "string", "description": "技能名稱" },
                "trigger":        { "type": "string", "description": "觸發關鍵詞，多個以逗號分隔" },
                "behavior":       { "type": "string", "description": "行為描述；工具鏈以 @[tool_name] 標記順序，例如：先 @[search_vault] 搜尋，再 @[plan_announce] 確認，最後 @[update_note] 更新" },
                "injection_mode": { "type": "string", "description": "passive（依關鍵字觸發）/ active（每次觸發）/ proactive（背景預載）" }
            }, "required": ["title", "trigger", "behavior"] }
        }}),
        // ── Memory agent tools ───────────────────────────────────────────────
        json!({ "type": "function", "function": {
            "name": "get_unprocessed_conversations",
            "description": "取得尚未處理的對話列表，用於記憶提煉",
            "parameters": { "type": "object", "properties": {
                "limit": { "type": "number", "description": "最多幾條，預設 20" }
            }, "required": [] }
        }}),
        json!({ "type": "function", "function": {
            "name": "get_conversation_content",
            "description": "取得指定對話的訊息內容",
            "parameters": { "type": "object", "properties": {
                "conversation_id": { "type": "string" },
                "skip_count":      { "type": "number", "description": "跳過前幾則" },
                "char_limit":      { "type": "number", "description": "字元限制，預設 500" }
            }, "required": ["conversation_id"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "save_memory_facts",
            "description": "儲存從對話中提煉的記憶事實",
            "parameters": { "type": "object", "properties": {
                "conversation_id": { "type": "string" },
                "facts": { "type": "array", "items": { "type": "object" } }
            }, "required": ["conversation_id", "facts"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "mark_conversation_processed",
            "description": "標記對話已完成記憶提煉",
            "parameters": { "type": "object", "properties": {
                "conversation_id": { "type": "string" }
            }, "required": ["conversation_id"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "condense_memory_facts",
            "description": "壓縮並合併同類記憶事實",
            "parameters": { "type": "object", "properties": {
                "category": { "type": "string", "description": "只壓縮指定類別，省略則全部" }
            }, "required": [] }
        }}),
        json!({ "type": "function", "function": {
            "name": "call_agent",
            "description": "呼叫另一個已定義的 agent 執行特定任務，並取回結果",
            "parameters": { "type": "object", "properties": {
                "name":  { "type": "string", "description": "agent 定義的名稱" },
                "input": { "type": "string", "description": "傳給 sub-agent 的任務描述或問題" }
            }, "required": ["name", "input"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "think",
            "description": "輸出一句內心獨白（10字以內），在呼叫工具前描述正在想什麼",
            "parameters": { "type": "object", "properties": {
                "thought": { "type": "string", "description": "內心獨白" }
            }, "required": ["thought"] }
        }}),
        json!({ "type": "function", "function": {
            "name": "live_respond",
            "description": "輸出語音助理的最終口語回覆（live chat 專用，呼叫後對話結束）",
            "parameters": { "type": "object", "properties": {
                "speech":  { "type": "string", "description": "TTS 朗讀文字，口語化，2 句以內，不含 Markdown" },
                "action":  { "type": "string", "description": "show_results / open_note / open_tab / show_error / none" },
                "content": { "type": "string", "description": "若有查到資料，把完整摘要放此供畫面顯示（可含換行）" }
            }, "required": ["speech", "action"] }
        }}),
    ];

    all_tools.into_iter()
        .filter(|t| {
            let name = t["function"]["name"].as_str().unwrap_or("");
            tool_names.iter().any(|n| n == name)
        })
        .collect()
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

/// Stream one LLM round, emitting llm:token events. Returns (text, finish_reason, tool_chunks).
/// tool_chunks: Vec<(id, name, arguments_str)>
pub(crate) async fn stream_llm_round(
    client: &reqwest::Client,
    llm_url: &str,
    body: Value,
    state: &ApiState,
    _session_id: &str,
    cancel: &Arc<AtomicBool>,
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
                                    pending_emit.push_str(content);
                                    if pending_emit.contains("<tool_call>") {
                                        // Emit only the text before the tag, then suppress
                                        let pos = pending_emit.find("<tool_call>").unwrap_or(0);
                                        if pos > 0 {
                                            state.daemon.emit("llm:token", json!(&pending_emit[..pos]));
                                        }
                                        pending_emit.clear();
                                        suppress_emit = true;
                                    } else {
                                        // Safe to emit up to a point where no partial tag can start
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
