use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use serde_json::{json, Value};
use crate::db::SurrealDb;
use chrono::Datelike;
use crate::service::harness::governance::guard::validate_rel_path;

// ── Structured error helper ───────────────────────────────────────────────────

/// Build a machine-readable tool error value.
/// `code`    — short all-caps identifier, e.g. "NOT_FOUND", "VAULT_NOT_SET".
/// `message` — human-readable explanation forwarded to the LLM.
/// `path`    — optional vault-relative path involved in the error.
#[inline]
fn tool_err(code: &str, message: impl Into<String>, path: Option<&str>) -> Value {
    let mut v = json!({ "error_code": code, "message": message.into() });
    if let Some(p) = path {
        v["path"] = json!(p);
    }
    v
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

pub(crate) fn vault_read_note(rel_path: &str, vault_path: &str) -> Value {
    if vault_path.is_empty() {
        return tool_err("VAULT_NOT_SET", "Vault 未設定，請先設定 Vault 路徑", None);
    }
    if rel_path.is_empty() {
        return tool_err("PATH_EMPTY", "路徑不能為空", None);
    }
    let full = std::path::Path::new(vault_path).join(rel_path);
    if !full.exists() {
        return tool_err("NOT_FOUND", format!("找不到筆記：{}", rel_path), Some(rel_path));
    }
    match std::fs::read_to_string(&full) {
        Ok(c)  => json!({ "error_code": null, "content": c, "path": rel_path }),
        Err(e) => tool_err("READ_FAILED", format!("讀取失敗：{}", e), Some(rel_path)),
    }
}

/// Search within a single note, returning matching paragraphs with their starting line numbers.
/// A "paragraph" is a block of consecutive non-empty lines separated by blank lines or markdown headers.
pub(crate) fn vault_search_in_note(rel_path: &str, query: &str, vault_path: &str) -> serde_json::Value {
    if vault_path.is_empty() { return json!("Vault 未設定"); }
    if rel_path.is_empty()   { return json!("路徑為空"); }
    if query.is_empty()      { return json!("查詢不能為空"); }

    let full = std::path::Path::new(vault_path).join(rel_path);
    let content = match std::fs::read_to_string(&full) {
        Ok(c)  => c,
        Err(_) => return json!(format!("讀取失敗：{}", rel_path)),
    };

    let q_lower = query.to_lowercase();
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut line_num: usize = 1;
    let mut para_start: usize = 1;
    let mut para_lines: Vec<&str> = Vec::new();

    let flush = |para_start: usize, lines: &Vec<&str>, results: &mut Vec<serde_json::Value>, q: &str| {
        if lines.is_empty() { return; }
        let text = lines.join("\n");
        if text.to_lowercase().contains(q) {
            results.push(json!({ "line": para_start, "text": text }));
        }
    };

    for line in content.lines() {
        // Start a new paragraph on blank lines or markdown headers (# / ##).
        let is_break = line.trim().is_empty() || line.starts_with('#');
        if is_break {
            flush(para_start, &para_lines, &mut results, &q_lower);
            para_lines.clear();
            // A header line itself is the first line of the next paragraph.
            if line.starts_with('#') {
                para_start = line_num;
                para_lines.push(line);
            } else {
                para_start = line_num + 1;
            }
        } else {
            if para_lines.is_empty() { para_start = line_num; }
            para_lines.push(line);
        }
        line_num += 1;
    }
    flush(para_start, &para_lines, &mut results, &q_lower);

    let total = results.len();
    if total == 0 {
        json!({ "matches": [], "message": format!("在 {} 中找不到「{}」", rel_path, query) })
    } else {
        json!({ "matches": results, "total": total })
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
    if let Some(vec) = crate::embedding::embedder::embed_text(client, embedding_url, query).await {
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
        if let Some(query_vec) = crate::embedding::embedder::embed_text(client, embedding_url, &query_text).await {
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
                        let score = crate::embedding::embedder::cosine_sim(&query_vec, emb);
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

// ── Readonly query tools ──────────────────────────────────────────────────────

pub(crate) async fn vault_list_recent_notes(
    db: &SurrealDb,
    vault_id: &str,
    limit: u64,
) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Row { path: String, title: String, updated_at: Option<i64> }
    let limit = limit.min(50);
    let mut resp = db
        .query("SELECT path, title, updated_at FROM notes WHERE vault_id = $vid ORDER BY updated_at DESC LIMIT $lim")
        .bind(("vid", vault_id.to_string()))
        .bind(("lim", limit))
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;
    Ok(json!(rows.iter().map(|r| json!({
        "path": r.path,
        "title": r.title,
        "updated_at": r.updated_at,
    })).collect::<Vec<_>>()))
}

pub(crate) async fn vault_search_by_tag(
    db: &SurrealDb,
    vault_id: &str,
    tag: &str,
) -> Result<Value, String> {
    if tag.is_empty() { return Ok(json!([])); }
    #[derive(serde::Deserialize)]
    struct Row { path: String, title: String }
    // Match inline #tag and frontmatter `tags: [...tag...]` line.
    // Using a frontmatter-specific pattern for the second condition avoids false
    // positives from notes that merely *mention* the tag word in their body.
    let hashtag = format!("#{}", tag);
    let escaped_tag: String = tag.chars().flat_map(|c| {
        if r"\.^$*+?()[]{}|".contains(c) { vec!['\\', c] } else { vec![c] }
    }).collect();
    let fm_pattern = format!(r"(?i)tags:[^\n]*\b{}\b", escaped_tag);
    let mut resp = db
        .query("SELECT path, title FROM notes WHERE vault_id = $vid AND (content ~ $ht OR content ~ $fm) LIMIT 20")
        .bind(("vid", vault_id.to_string()))
        .bind(("ht", hashtag))
        .bind(("fm", fm_pattern))
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;
    Ok(json!(rows.iter().map(|r| json!({"path": r.path, "title": r.title})).collect::<Vec<_>>()))
}

pub(crate) async fn vault_get_stats(
    db: &SurrealDb,
    vault_id: &str,
    vault_path: &str,
) -> Result<Value, String> {
    // Count notes by querying only the path column (lightweight).
    #[derive(serde::Deserialize)]
    struct PathRow { #[allow(dead_code)] path: String }
    let note_count = db
        .query("SELECT path FROM notes WHERE vault_id = $vid")
        .bind(("vid", vault_id.to_string()))
        .await.ok()
        .and_then(|mut r| r.take::<Vec<PathRow>>(0).ok())
        .map(|rows| rows.len())
        .unwrap_or(0);
    let folder_count = if !vault_path.is_empty() {
        count_directories(std::path::Path::new(vault_path), 0)
    } else { 0 };
    Ok(json!({ "note_count": note_count, "folder_count": folder_count }))
}

fn count_directories(dir: &std::path::Path, depth: u32) -> u32 {
    if depth > 4 { return 0; }
    let Ok(entries) = std::fs::read_dir(dir) else { return 0; };
    let mut count = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') { continue; }
        if path.is_dir() {
            count += 1 + count_directories(&path, depth + 1);
        }
    }
    count
}

pub(crate) async fn vault_get_note_backlinks(
    db: &SurrealDb,
    vault_id: &str,
    rel_path: &str,
) -> Result<Value, String> {
    if rel_path.is_empty() { return Ok(json!([])); }
    #[derive(serde::Deserialize)]
    struct Row { path: String, title: String }
    // Build a regex that matches [[stem]] or [[stem|alias]] or [[folder/stem...]].
    let stem = rel_path.split('/').last().unwrap_or(rel_path).trim_end_matches(".md");
    if stem.is_empty() { return Ok(json!([])); }
    // Escape basic regex special chars in the stem.
    let escaped: String = stem.chars().flat_map(|c| {
        if r"\.^$*+?()[]{}|".contains(c) { vec!['\\', c] } else { vec![c] }
    }).collect();
    let pattern = format!(r"(?i)\[\[([^|\]]*/)?({})[|\]]", escaped);
    let mut resp = db
        .query("SELECT path, title FROM notes WHERE vault_id = $vid AND path != $p AND content ~ $pat LIMIT 30")
        .bind(("vid", vault_id.to_string()))
        .bind(("p", rel_path.to_string()))
        .bind(("pat", pattern))
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;
    Ok(json!(rows.iter().map(|r| json!({"path": r.path, "title": r.title})).collect::<Vec<_>>()))
}

pub(crate) async fn vault_find_orphan_notes(
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Row { path: String, title: String, content: Option<String> }
    let mut resp = db
        .query("SELECT path, title, content FROM notes WHERE vault_id = $vid")
        .bind(("vid", vault_id.to_string()))
        .await
        .map_err(|e| e.to_string())?;
    let rows: Vec<Row> = resp.take(0).map_err(|e| e.to_string())?;
    // Collect every wikilink stem referenced across all notes.
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in &rows {
        if let Some(content) = &row.content {
            for stem in extract_wikilinks(content) {
                referenced.insert(stem);
            }
        }
    }
    // A note is an orphan if its own stem is never referenced.
    let orphans: Vec<Value> = rows.iter()
        .filter(|r| {
            let stem = r.path.split('/').last().unwrap_or(&r.path)
                .trim_end_matches(".md")
                .to_lowercase();
            !referenced.contains(&stem)
        })
        .map(|r| json!({"path": r.path, "title": r.title}))
        .collect();
    Ok(json!(orphans))
}

/// Extract the lowercased stem of every `[[...]]` wikilink in `content`.
fn extract_wikilinks(content: &str) -> Vec<String> {
    let mut links = vec![];
    let mut remaining = content;
    while let Some(start) = remaining.find("[[") {
        let rest = &remaining[start + 2..];
        if let Some(end) = rest.find("]]") {
            let inner = &rest[..end];
            // Handle [[target|alias]] — use only the target part.
            let target = inner.split('|').next().unwrap_or(inner).trim();
            // Use just the last path component (no folder prefix, no .md).
            let stem = target.split('/').last().unwrap_or(target)
                .trim_end_matches(".md")
                .to_lowercase();
            if !stem.is_empty() { links.push(stem); }
            remaining = &rest[end + 2..];
        } else {
            break;
        }
    }
    links
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
    if vault_path.is_empty() { return Ok(tool_err("VAULT_NOT_SET", "Vault 未設定", None)); }
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

/// Variant that checks for write conflicts before committing.
/// `mtime_at_read` — the file's mtime (secs) when the caller last read it.
/// If the file's current mtime is newer, returns a conflict result without writing.
pub(crate) async fn vault_update_note_with_conflict_check(
    rel_path: &str,
    content: &str,
    original: &str,
    mtime_at_read: u64,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Ok(tool_err("VAULT_NOT_SET", "Vault 未設定", None)); }
    validate_rel_path(rel_path)?;
    let full = std::path::Path::new(vault_path).join(rel_path);
    if !full.exists() { return Ok(tool_err("NOT_FOUND", format!("筆記不存在：{}", rel_path), Some(rel_path))); }

    // Conflict check: if file mtime has advanced since we read, someone else modified it.
    if mtime_at_read > 0 {
        let current_mtime = tokio::fs::metadata(&full).await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if current_mtime > mtime_at_read {
            return Ok(json!({
                "error_code": "WRITE_CONFLICT",
                "conflict":   true,
                "path":       rel_path,
                "message":    format!(
                    "檔案 {} 在你讀取後已被修改（讀取時 mtime={}, 現在={}）。\
                     請用 read_note 重新讀取最新內容，或用 ask_user 詢問使用者要如何合併。",
                    rel_path, mtime_at_read, current_mtime
                ),
            }));
        }
    }
    vault_update_note_inner(rel_path, content, original, &full, client, db, vault_id).await
}


async fn vault_update_note_inner(
    rel_path: &str,
    content: &str,
    original: &str,
    full: &std::path::Path,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    tokio::fs::write(full, content).await.map_err(|e| e.to_string())?;
    sync_note_to_db(client, db, vault_id, rel_path, content).await;
    let lines_before = original.lines().count();
    let lines_after  = content.lines().count();
    Ok(json!({
        "ok":            true,
        "path":          rel_path,
        "lines_before":  lines_before,
        "lines_after":   lines_after,
        "lines_added":   lines_after.saturating_sub(lines_before),
        "lines_removed": lines_before.saturating_sub(lines_after),
    }))
}

/// Read a note and immediately write new content in one round-trip.
/// Injects a synthetic `read_note` record into `working_memory` so that any
/// subsequent `update_note` guard on the same path is automatically satisfied.
pub(crate) async fn vault_read_then_write(
    rel_path: &str,
    new_content: &str,
    mtime_hint: u64,        // mtime recorded just before this call (0 = skip conflict check)
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
    working_memory: &super::super::memory::working::WorkingMemory,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Ok(tool_err("VAULT_NOT_SET", "Vault 未設定", None)); }
    validate_rel_path(rel_path)?;
    let full = std::path::Path::new(vault_path).join(rel_path);
    if !full.exists() { return Ok(tool_err("NOT_FOUND", format!("筆記不存在：{}", rel_path), Some(rel_path))); }
    let original = tokio::fs::read_to_string(&full).await.map_err(|e| e.to_string())?;

    // Conflict check against the mtime recorded just before calling this function.
    if mtime_hint > 0 {
        let current_mtime = tokio::fs::metadata(&full).await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if current_mtime > mtime_hint {
            return Ok(json!({
                "error_code": "WRITE_CONFLICT",
                "conflict":   true,
                "path":       rel_path,
                "message":    format!(
                    "檔案 {} 在操作過程中已被修改。請重新讀取後再寫入。",
                    rel_path
                ),
            }));
        }
    }

    // Inject synthetic read_note record so future update_note guards are satisfied.
    let synthetic_id = format!("rtw_read_{}", rel_path);
    working_memory.record(
        synthetic_id,
        "read_note",
        json!({ "path": rel_path }),
        json!({ "error_code": null, "content": original, "path": rel_path }),
        chrono::Utc::now().timestamp(),
        0,
        crate::service::harness::governance::guard::GuardOutcome::Exempt,
    ).await;

    let original_chars = original.len();
    let mut result = vault_update_note_inner(
        rel_path, new_content, &original, &full, client, db, vault_id,
    ).await?;
    // Augment result with original_chars for the agent to know how much was replaced.
    if let Some(obj) = result.as_object_mut() {
        obj.insert("original_chars".to_string(), json!(original_chars));
    }
    Ok(result)
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
    if vault_path.is_empty() { return Ok(tool_err("VAULT_NOT_SET", "Vault 未設定", None)); }
    validate_rel_path(rel_path)?;
    let full = std::path::Path::new(vault_path).join(rel_path);
    if !full.exists() { return Ok(tool_err("NOT_FOUND", format!("筆記不存在：{}", rel_path), Some(rel_path))); }
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

// ── Frontmatter update ────────────────────────────────────────────────────────

/// Update YAML frontmatter fields in a note without touching the body.
/// If the note has no frontmatter, prepend a new block with the given fields.
/// `fields` must be a JSON object; array values are serialised as `[a, b, c]`.
pub(crate) async fn vault_update_note_frontmatter(
    rel_path: &str,
    fields: &Value,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    validate_rel_path(rel_path)?;
    if !fields.is_object()   { return Err("fields 必須為物件（鍵值對）".to_string()); }

    let abs = std::path::PathBuf::from(vault_path).join(rel_path);
    let original = tokio::fs::read_to_string(&abs).await
        .map_err(|e| format!("讀取失敗：{}", e))?;

    let new_content = if original.starts_with("---") {
        let after_first = &original[3..];
        if let Some(end_pos) = after_first.find("\n---") {
            let fm_content = &after_first[..end_pos];
            let body = &after_first[end_pos + 4..];
            let mut lines: Vec<String> = fm_content.lines().map(|l| l.to_string()).collect();
            if let Some(obj) = fields.as_object() {
                for (key, val) in obj {
                    let val_str = match val {
                        Value::String(s) => s.clone(),
                        Value::Array(arr) => {
                            let items: Vec<String> = arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect();
                            format!("[{}]", items.join(", "))
                        }
                        v => v.to_string(),
                    };
                    let key_prefix = format!("{}:", key);
                    let mut found = false;
                    for line in &mut lines {
                        if line.starts_with(&key_prefix) {
                            *line = format!("{}: {}", key, val_str);
                            found = true;
                            break;
                        }
                    }
                    if !found { lines.push(format!("{}: {}", key, val_str)); }
                }
            }
            format!("---\n{}\n---{}", lines.join("\n"), body)
        } else {
            // Malformed frontmatter (opening --- but no closing) — leave as-is.
            original
        }
    } else {
        // No frontmatter — prepend a new block.
        let mut fm = vec!["---".to_string()];
        if let Some(obj) = fields.as_object() {
            for (key, val) in obj {
                let vs = match val {
                    Value::String(s) => s.clone(),
                    v => v.to_string(),
                };
                fm.push(format!("{}: {}", key, vs));
            }
        }
        fm.push("---".to_string());
        fm.push(String::new());
        fm.push(original);
        fm.join("\n")
    };

    tokio::fs::write(&abs, &new_content).await
        .map_err(|e| format!("寫入失敗：{}", e))?;

    sync_note_to_db(client, db, vault_id, rel_path, &new_content).await;

    Ok(json!(format!("✅ 已更新 {} 的 frontmatter 欄位", rel_path)))
}

// ── Web search (Brave Search API) ─────────────────────────────────────────────

const BRAVE_MONTHLY_LIMIT: u32 = 1000;

/// First 8 hex chars of SHA-256(api_key) — used to namespace per-key quota counters.
fn brave_key_id(api_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(api_key.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..4])
}

/// How many Brave searches have been used this calendar month for the given key.
async fn get_brave_used(db: &SurrealDb, key_id: &str) -> u32 {
    let current_month = chrono::Utc::now().format("%Y-%m").to_string();
    #[derive(serde::Deserialize)]
    struct Row { value: String }

    let month_key = format!("brave_search_month_{}", key_id);
    let used_key  = format!("brave_search_used_{}", key_id);

    let stored_month: String = db
        .query("SELECT `value` FROM `settings` WHERE `key` = $k LIMIT 1")
        .bind(("k", month_key))
        .await.ok()
        .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .map(|r| r.value)
        .unwrap_or_default();

    if stored_month != current_month { return 0; }

    db.query("SELECT `value` FROM `settings` WHERE `key` = $k LIMIT 1")
        .bind(("k", used_key))
        .await.ok()
        .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .and_then(|r| r.value.parse().ok())
        .unwrap_or(0)
}

/// Increment (or reset) the monthly Brave search counter.
async fn increment_brave_used(db: &SurrealDb, key_id: &str) {
    let current_month = chrono::Utc::now().format("%Y-%m").to_string();
    let month_key = format!("brave_search_month_{}", key_id);
    let used_key  = format!("brave_search_used_{}", key_id);
    let now = chrono::Utc::now().timestamp();

    let new_used = get_brave_used(db, key_id).await + 1;

    // Upsert month marker
    upsert_setting_db(db, &month_key, &current_month, now).await;
    // Upsert used counter
    upsert_setting_db(db, &used_key, &new_used.to_string(), now).await;
}

/// Simple upsert for the `settings` table (SELECT + INSERT/UPDATE).
async fn upsert_setting_db(db: &SurrealDb, key: &str, value: &str, now: i64) {
    #[derive(serde::Deserialize)]
    struct IdRow { id: surrealdb::RecordId }
    let existing: Vec<IdRow> = db
        .query("SELECT id FROM `settings` WHERE `key` = $k LIMIT 1")
        .bind(("k", key.to_string()))
        .await.ok()
        .and_then(|mut r| r.take(0).ok())
        .unwrap_or_default();
    if existing.is_empty() {
        let _ = db
            .query("INSERT INTO `settings` (`key`, `value`, updated_at) VALUES ($k, $v, $now)")
            .bind(("k", key.to_string()))
            .bind(("v", value.to_string()))
            .bind(("now", now))
            .await;
    } else {
        let id = existing.into_iter().next().unwrap().id;
        let _ = db
            .query("UPDATE $id SET `value` = $v, updated_at = $now")
            .bind(("id", id))
            .bind(("v", value.to_string()))
            .bind(("now", now))
            .await;
    }
}

/// Call the Brave Search API. Returns (title, url, snippet) tuples.
async fn brave_search_api(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
) -> Result<Vec<(String, String, String)>, String> {
    #[derive(serde::Deserialize)]
    struct BraveResult { title: Option<String>, url: Option<String>, description: Option<String> }
    #[derive(serde::Deserialize)]
    struct BraveWeb { results: Option<Vec<BraveResult>> }
    #[derive(serde::Deserialize)]
    struct BraveResponse { web: Option<BraveWeb> }

    let response = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .query(&[("q", query), ("count", "5")])
        .send().await
        .map_err(|e| format!("網路請求失敗：{}", e))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Brave API 回傳 HTTP {}：{}", status, body));
    }

    let resp: BraveResponse = response.json().await
        .map_err(|e| format!("解析 Brave API 回應失敗：{}", e))?;

    Ok(resp.web
        .and_then(|w| w.results)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let url = r.url.unwrap_or_default();
            if url.is_empty() { return None; }
            Some((r.title.unwrap_or_default(), url, r.description.unwrap_or_default()))
        })
        .collect())
}

/// Web search tool handler: reads Brave API key from DB, checks quota, calls API.
pub(crate) async fn vault_web_search(
    client: &reqwest::Client,
    db: &SurrealDb,
    state_emit: impl Fn(&str, serde_json::Value),
    session_id: &str,
    query: &str,
) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct Row { value: String }

    // Read encrypted Brave API key
    let enc: String = db
        .query("SELECT `value` FROM `settings` WHERE `key` = 'api_key_brave_search' LIMIT 1")
        .await.ok()
        .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .map(|r| r.value)
        .unwrap_or_default();

    if enc.is_empty() {
        return Ok(json!("請至設定頁面設定 Brave Search API Key"));
    }

    let api_key = crate::service::harness::crypto::decrypt_api_key_db(db, &enc).await;
    if api_key.is_empty() {
        return Ok(json!("Brave Search API Key 解密失敗，請重新設定"));
    }

    let key_id = brave_key_id(&api_key);
    let used = get_brave_used(db, &key_id).await;
    if used >= BRAVE_MONTHLY_LIMIT {
        let next_month = {
            let m = chrono::Utc::now().month();
            if m == 12 { 1 } else { m + 1 }
        };
        return Ok(json!(format!(
            "已達每月搜尋上限（{}/{}），{}月1號重置。",
            used, BRAVE_MONTHLY_LIMIT, next_month
        )));
    }

    let results = brave_search_api(client, &api_key, query).await?;

    if results.is_empty() {
        return Ok(json!(format!("Brave Search 未找到「{}」的搜尋結果。", query)));
    }

    increment_brave_used(db, &key_id).await;

    // Emit web refs for frontend "儲存為知識" button
    let refs: Vec<Value> = results.iter().take(3).map(|(title, url, snippet)| {
        json!({ "path": url, "title": title, "excerpt": snippet })
    }).collect();
    state_emit("agent:web_refs", json!({
        "session_id": session_id,
        "refs": refs,
    }));

    let formatted = results.iter().enumerate()
        .map(|(i, (title, url, snippet))| {
            format!("[{}] **{}**\n{}\n來源：{}", i + 1, title, snippet, url)
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    Ok(json!(format!("搜尋「{}」的結果：\n\n{}", query, formatted)))
}

// ── Wikilink insertion ────────────────────────────────────────────────────────

/// Insert a [[target]] wikilink into the source note's "相關筆記" section.
pub(crate) async fn vault_link_notes(
    source: &str,
    target: &str,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    validate_rel_path(source)?;
    validate_rel_path(target)?;
    let abs = std::path::Path::new(vault_path).join(source);
    if !abs.exists() { return Err(format!("筆記不存在：{}", source)); }
    let content = tokio::fs::read_to_string(&abs).await.map_err(|e| e.to_string())?;
    let link_title = target.trim_end_matches(".md");
    let wikilink = format!("[[{}]]", link_title);
    if content.contains(&wikilink) {
        return Ok(json!(format!("⚠️ {} 已包含 {} 的連結，無需重複新增。", source, wikilink)));
    }
    let new_content = if content.contains("## 相關筆記") {
        format!("{}\n- {}", content.trim_end(), wikilink)
    } else {
        format!("{}\n\n## 相關筆記\n\n- {}", content.trim_end(), wikilink)
    };
    tokio::fs::write(&abs, &new_content).await.map_err(|e| format!("寫入失敗：{}", e))?;
    sync_note_to_db(client, db, vault_id, source, &new_content).await;
    Ok(json!(format!("✅ 已在 {} 中新增 {} 的連結", source, wikilink)))
}

// ── Knowledge compression ─────────────────────────────────────────────────────

/// Save an important insight or knowledge snippet to the knowledge/ folder.
pub(crate) async fn vault_compress_to_knowledge(
    title: &str,
    content: &str,
    tags: &[String],
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    if title.is_empty() { return Err("標題不能為空".to_string()); }
    let safe = title
        .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_")
        .to_lowercase();
    validate_rel_path(&safe)?;
    let knowledge_dir = std::path::Path::new(vault_path).join("knowledge");
    tokio::fs::create_dir_all(&knowledge_dir).await.map_err(|e| e.to_string())?;
    let rel_path = format!("knowledge/{}.md", safe);
    let abs = knowledge_dir.join(format!("{}.md", safe));
    if abs.exists() { return Err(format!("知識筆記已存在：{}", rel_path)); }
    let date_str = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let fm_content = if !tags.is_empty() {
        let tags_str: Vec<String> = tags.iter().map(|s| format!("\"{}\"", s)).collect();
        format!("---\ntitle: {}\ndate: {}\ntags: [{}]\n---\n\n{}", title, date_str, tags_str.join(", "), content)
    } else {
        format!("---\ntitle: {}\ndate: {}\n---\n\n{}", title, date_str, content)
    };
    tokio::fs::write(&abs, &fm_content).await.map_err(|e| e.to_string())?;
    sync_note_to_db(client, db, vault_id, &rel_path, &fm_content).await;
    Ok(json!({ "ok": true, "path": rel_path }))
}

// ── Map of Contents generation ────────────────────────────────────────────────

/// Generate a Map of Contents for a folder and write it to {folder}/_moc.md.
pub(crate) async fn vault_generate_moc(
    folder: &str,
    title: Option<&str>,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    validate_rel_path(folder)?;
    let base = std::path::Path::new(vault_path);
    let folder_abs = base.join(folder);
    if !folder_abs.is_dir() { return Err(format!("資料夾不存在：{}", folder)); }

    fn scan_dir(dir: &std::path::Path, base: &std::path::Path, depth: u32) -> Vec<String> {
        if depth > 2 { return vec![]; }
        let Ok(entries) = std::fs::read_dir(dir) else { return vec![]; };
        let mut paths: Vec<_> = entries.flatten().collect();
        paths.sort_by_key(|e| e.file_name());
        let mut results = vec![];
        for entry in paths {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.to_lowercase() == "_moc.md" { continue; }
            if path.is_dir() {
                results.extend(scan_dir(&path, base, depth + 1));
            } else if name.ends_with(".md") {
                let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().to_string();
                results.push(rel);
            }
        }
        results
    }

    let notes = scan_dir(&folder_abs, base, 0);
    let moc_title = title.unwrap_or(folder);
    let date_str = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut lines = vec![
        format!("---\ntitle: {}\ntype: moc\nupdated: {}\n---", moc_title, date_str),
        String::new(),
        format!("# {}", moc_title),
        String::new(),
    ];
    if notes.is_empty() {
        lines.push("（此資料夾目前沒有筆記）".to_string());
    } else {
        for note_path in &notes {
            let stem = note_path.split('/').last().unwrap_or(note_path).trim_end_matches(".md");
            lines.push(format!("- [[{}]]", stem));
        }
    }
    let moc_content = lines.join("\n");
    let moc_rel = format!("{}/_moc.md", folder);
    let moc_abs = base.join(&moc_rel);
    tokio::fs::write(&moc_abs, &moc_content).await.map_err(|e| format!("寫入失敗：{}", e))?;
    sync_note_to_db(client, db, vault_id, &moc_rel, &moc_content).await;
    Ok(json!({ "ok": true, "path": moc_rel, "notes_count": notes.len() }))
}

// ── Task scheduling ───────────────────────────────────────────────────────────

/// Create a task note in the tasks/ folder with YAML frontmatter.
pub(crate) async fn vault_schedule_task(
    title: &str,
    description: &str,
    due_date: Option<&str>,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    if title.is_empty() { return Err("任務標題不能為空".to_string()); }
    let safe = title
        .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_")
        .to_lowercase();
    validate_rel_path(&safe)?;
    let tasks_dir = std::path::Path::new(vault_path).join("tasks");
    tokio::fs::create_dir_all(&tasks_dir).await.map_err(|e| e.to_string())?;
    let rel_path = format!("tasks/{}.md", safe);
    let abs = tasks_dir.join(format!("{}.md", safe));
    if abs.exists() { return Err(format!("任務筆記已存在：{}", rel_path)); }
    let created_str = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let due_line = due_date.map(|d| format!("\ndue: {}", d)).unwrap_or_default();
    let content = format!(
        "---\ntitle: {}\nstatus: todo\ncreated: {}{}\n---\n\n{}\n",
        title, created_str, due_line, description
    );
    tokio::fs::write(&abs, &content).await.map_err(|e| e.to_string())?;
    sync_note_to_db(client, db, vault_id, &rel_path, &content).await;
    Ok(json!({ "ok": true, "path": rel_path }))
}

/// Extract note paths for agent:note_refs event
pub(crate) fn extract_note_refs(tool_name: &str, args: &Value, _result: &Value, _vault_path: &str) -> Vec<String> {
    match tool_name {
        "read_note" => {
            let p = args["path"].as_str().unwrap_or("");
            if p.is_empty() { return vec![]; }
            let lower = p.to_lowercase();
            let full = if lower.ends_with(".md") { lower } else { format!("{}.md", lower) };
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
