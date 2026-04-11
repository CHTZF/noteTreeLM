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

    fn fmt_size(bytes: u64) -> String {
        if bytes >= 1_024 * 1_024 {
            format!("{:.1} MB", bytes as f64 / (1_024.0 * 1_024.0))
        } else if bytes >= 1_024 {
            format!("{:.1} KB", bytes as f64 / 1_024.0)
        } else {
            format!("{} B", bytes)
        }
    }

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
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                out.push_str(&format!("{}{} ({})\n", indent, rel, fmt_size(size)));
            }
        }
        out
    }

    list_dir(&target, base, 0)
}

pub(crate) fn vault_read_note(
    rel_path:   &str,
    vault_path: &str,
    offset:     Option<usize>,
    limit:      Option<usize>,
) -> Value {
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
        Err(e) => tool_err("READ_FAILED", format!("讀取失敗：{}", e), Some(rel_path)),
        Ok(raw) => {
            let total_lines = raw.lines().count();
            let content = if offset.is_some() || limit.is_some() {
                let off = offset.unwrap_or(0);
                let lines: Vec<&str> = raw.lines().skip(off).take(limit.unwrap_or(usize::MAX)).collect();
                lines.join("\n")
            } else {
                raw
            };
            let has_more = if let (_, Some(lim)) = (offset.unwrap_or(0), limit) {
                offset.unwrap_or(0) + lim < total_lines
            } else {
                false
            };
            json!({
                "error_code":  null,
                "content":     content,
                "path":        rel_path,
                "total_lines": total_lines,
                "has_more":    has_more,
            })
        }
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
    Ok(json!(rows.iter().map(|r| json!({"path": r.path, "title": r.title, "score": null})).collect::<Vec<_>>()))
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

    // Fallback: keyword regex or recency sort.
    // For CJK keywords, enrich with bigrams to improve substring-match recall.
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
        use crate::service::harness::memory::semantic::enrich_keywords_for_fallback;
        let enriched = enrich_keywords_for_fallback(keywords);
        let mut collected: Vec<Row> = Vec::new();
        for kw in enriched.iter().take(8) {
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
        // Deduplicate by fact_id.
        let mut seen = std::collections::HashSet::new();
        collected.retain(|r| {
            let fid = r.fact_id.clone().unwrap_or_default();
            if fid.is_empty() { return true; }
            seen.insert(fid)
        });
        collected.truncate(limit as usize);
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

/// Replace the first occurrence of `old_text` with `new_text` in a note.
/// Returns an error if `old_text` is not found (prevents silent mis-edits).
/// Cheaper than a full `update_note` round-trip: caller does not need to read
/// the entire file first.
pub(crate) async fn vault_patch_note(
    rel_path: &str,
    old_text: &str,
    new_text: &str,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Ok(tool_err("VAULT_NOT_SET", "Vault 未設定", None)); }
    validate_rel_path(rel_path)?;
    if old_text.is_empty() { return Ok(tool_err("EMPTY_OLD_TEXT", "old_text 不可為空", Some(rel_path))); }
    let full = std::path::Path::new(vault_path).join(rel_path);
    if !full.exists() { return Ok(tool_err("NOT_FOUND", format!("筆記不存在：{}", rel_path), Some(rel_path))); }

    let original = tokio::fs::read_to_string(&full).await.map_err(|e| e.to_string())?;
    if !original.contains(old_text) {
        return Ok(tool_err("NOT_FOUND_IN_FILE",
            format!("在 {} 中找不到指定的 old_text（前50字：{:?}）", rel_path, old_text.chars().take(50).collect::<String>()),
            Some(rel_path),
        ));
    }

    let patched = original.replacen(old_text, new_text, 1);
    tokio::fs::write(&full, &patched).await.map_err(|e| e.to_string())?;
    sync_note_to_db(client, db, vault_id, rel_path, &patched).await;

    Ok(json!({
        "ok":            true,
        "path":          rel_path,
        "lines_before":  original.lines().count(),
        "lines_after":   patched.lines().count(),
    }))
}

/// Replace lines `start_line..=end_line` (1-indexed, inclusive) in a note with `new_text`.
///
/// Preferred over `patch_note` when the agent knows which lines to change but cannot
/// guarantee an exact string match (e.g., after reading with line numbers, or when the
/// selection spans a predictable block that may contain minor whitespace variation).
///
/// `new_text` replaces the entire line range; it may contain any number of lines.
/// Returns an error if the line range is out of bounds.
pub(crate) async fn vault_line_range_patch(
    rel_path: &str,
    start_line: usize, // 1-indexed
    end_line:   usize, // 1-indexed, inclusive
    new_text:   &str,
    vault_path: &str,
    client: &reqwest::Client,
    db: &SurrealDb,
    vault_id: &str,
) -> Result<Value, String> {
    if vault_path.is_empty() { return Ok(tool_err("VAULT_NOT_SET", "Vault 未設定", None)); }
    validate_rel_path(rel_path)?;
    if start_line == 0 { return Ok(tool_err("INVALID_RANGE", "start_line 必須 ≥ 1", Some(rel_path))); }
    let full = std::path::Path::new(vault_path).join(rel_path);
    if !full.exists() { return Ok(tool_err("NOT_FOUND", format!("筆記不存在：{}", rel_path), Some(rel_path))); }

    let original = tokio::fs::read_to_string(&full).await.map_err(|e| e.to_string())?;
    let mut lines: Vec<&str> = original.split('\n').collect();
    // Remove trailing empty element that split('\n') produces for files ending with \n
    if lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }

    let total = lines.len();
    let s = start_line - 1; // convert to 0-indexed
    let e = end_line.min(total).saturating_sub(1); // 0-indexed, clamped

    if s > total {
        return Ok(tool_err("RANGE_OUT_OF_BOUNDS",
            format!("start_line={} 超出檔案總行數={}", start_line, total),
            Some(rel_path),
        ));
    }

    let mut result: Vec<&str> = Vec::with_capacity(total);
    result.extend_from_slice(&lines[..s]);
    // Insert new_text lines
    let new_lines: Vec<&str> = new_text.split('\n').collect();
    result.extend_from_slice(&new_lines);
    if e + 1 < lines.len() {
        result.extend_from_slice(&lines[e + 1..]);
    }

    // Re-join; preserve trailing newline if original had one
    let mut patched = result.join("\n");
    if original.ends_with('\n') { patched.push('\n'); }

    tokio::fs::write(&full, &patched).await.map_err(|e| e.to_string())?;
    sync_note_to_db(client, db, vault_id, rel_path, &patched).await;

    Ok(json!({
        "ok":            true,
        "path":          rel_path,
        "replaced_lines": format!("{}-{}", start_line, end_line.min(total)),
        "lines_before":  total,
        "lines_after":   patched.split('\n').count(),
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
    struct BraveResult {
        title: Option<String>,
        url: Option<String>,
        description: Option<String>,
        extra_snippets: Option<Vec<String>>,
    }
    #[derive(serde::Deserialize)]
    struct BraveWeb { results: Option<Vec<BraveResult>> }
    #[derive(serde::Deserialize)]
    struct BraveResponse { web: Option<BraveWeb> }

    let response = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .query(&[("q", query), ("count", "5"), ("extra_snippets", "true")])
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
            // Combine description + extra_snippets for richer context
            let mut snippet = r.description.unwrap_or_default();
            if let Some(extras) = r.extra_snippets {
                let extra_text: String = extras.into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !extra_text.is_empty() {
                    if snippet.is_empty() {
                        snippet = extra_text;
                    } else {
                        snippet = format!("{} {}", snippet, extra_text);
                    }
                }
            }
            Some((r.title.unwrap_or_default(), url, snippet))
        })
        .collect())
}

// ── KB page search ────────────────────────────────────────────────────────────

fn remove_block(html: &str, tag: &str) -> String {
    let open  = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut result = String::new();
    let mut remaining = html;
    loop {
        let lower = remaining.to_lowercase();
        match lower.find(&open) {
            Some(start) => {
                result.push_str(&remaining[..start]);
                match lower[start..].find(&close) {
                    Some(end) => { remaining = &remaining[start + end + close.len()..]; }
                    None => break,
                }
            }
            None => { result.push_str(remaining); break; }
        }
    }
    result
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
     .replace("&quot;", "\"").replace("&apos;", "'").replace("&nbsp;", " ")
     .replace("&#39;", "'").replace("&#34;", "\"")
}

fn html_to_markdown(html: &str) -> String {
    let html = remove_block(html, "script");
    let html = remove_block(&html, "style");
    let html = remove_block(&html, "nav");
    let html = remove_block(&html, "header");
    let html = remove_block(&html, "footer");

    let lower = html.to_lowercase();
    let bytes = html.as_bytes();
    let len   = bytes.len();
    let mut out = String::new();
    let mut buf = String::new();
    let mut i   = 0usize;

    while i < len {
        if bytes[i] == b'<' {
            let tag_end = lower[i..].find('>').map(|e| i + e + 1).unwrap_or(len);
            let tag_inner = &html[i + 1..tag_end - 1];
            let tag_lower = tag_inner.trim().to_lowercase();
            let is_closing = tag_lower.starts_with('/');
            let tag_name = tag_lower.trim_start_matches('/').split_whitespace().next().unwrap_or("").to_string();
            match tag_name.as_str() {
                "h1" if !is_closing => out.push_str("\n\n# "),
                "h2" if !is_closing => out.push_str("\n\n## "),
                "h3" if !is_closing => out.push_str("\n\n### "),
                "h4" if !is_closing => out.push_str("\n\n#### "),
                "h5" if !is_closing => out.push_str("\n\n##### "),
                "h6" if !is_closing => out.push_str("\n\n###### "),
                "h1"|"h2"|"h3"|"h4"|"h5"|"h6" if is_closing => out.push('\n'),
                "p" if !is_closing  => out.push_str("\n\n"),
                "p" if is_closing   => out.push('\n'),
                "br"                => out.push('\n'),
                "strong"|"b" if !is_closing => out.push_str("**"),
                "strong"|"b" if is_closing  => out.push_str("**"),
                "em"|"i" if !is_closing => out.push('*'),
                "em"|"i" if is_closing  => out.push('*'),
                "code" if !is_closing => out.push('`'),
                "code" if is_closing  => out.push('`'),
                "pre" if !is_closing  => out.push_str("\n\n```\n"),
                "pre" if is_closing   => out.push_str("\n```\n\n"),
                "li" if !is_closing   => out.push_str("\n- "),
                "ul"|"ol" if !is_closing => out.push('\n'),
                "ul"|"ol" if is_closing  => out.push('\n'),
                "a" if !is_closing => {
                    let href = {
                        let ti_lower = tag_inner.to_lowercase();
                        if let Some(hp) = ti_lower.find("href=") {
                            let after = &tag_inner[hp + 5..];
                            if after.starts_with('"') { after[1..].split('"').next().unwrap_or("").to_string() }
                            else if after.starts_with('\'') { after[1..].split('\'').next().unwrap_or("").to_string() }
                            else { after.split_whitespace().next().unwrap_or("").trim_end_matches('>').to_string() }
                        } else { String::new() }
                    };
                    out.push('[');
                    buf = href;
                }
                "a" if is_closing => { out.push_str("]("); out.push_str(&buf); out.push(')'); buf.clear(); }
                _ => {}
            }
            i = tag_end;
        } else {
            let text_end = lower[i..].find('<').map(|e| i + e).unwrap_or(len);
            out.push_str(&decode_entities(&html[i..text_end]));
            i = text_end;
        }
    }

    let mut final_lines: Vec<&str> = Vec::new();
    let mut blank_count = 0u32;
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() { blank_count += 1; if blank_count <= 2 { final_lines.push(""); } }
        else { blank_count = 0; final_lines.push(t); }
    }
    final_lines.join("\n").trim().to_string()
}

fn sha256_hex(content: &str) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(content.as_bytes()))
}

/// Search imported KB pages.
/// - source_type="kb_session" + source_id=<session_id>: filter to that import session
/// - otherwise: search all imported pages for this vault
/// Search order: embedding (cosine) → FTS fallback.
/// On-demand fetches pending pages matching by title if no imported results found.
/// Returns JSON array of `{ __cite_id__, url, title, content }`.
pub(crate) async fn vault_search_kb_pages(
    client:      &reqwest::Client,
    embedding_url: &Option<String>,
    db:          &SurrealDb,
    vault_id:    &str,
    source_type: Option<&str>,
    source_id:   Option<&str>,
    query:       &str,
) -> Result<Value, String> {
    #[derive(serde::Deserialize)]
    struct PageRow {
        page_id:      String,
        url:          String,
        title:        Option<String>,
        content_md:   Option<String>,
        session_id:   Option<String>,
        #[allow(dead_code)]
        content_hash: Option<String>,
    }

    impl AsPageRow for PageRow {
        fn page_id(&self)    -> &str          { &self.page_id }
        fn url(&self)        -> &str          { &self.url }
        fn title(&self)      -> Option<&str>  { self.title.as_deref() }
        fn content_md(&self) -> Option<&str>  { self.content_md.as_deref() }
    }

    let kb_session_id = if source_type == Some("kb_session") { source_id } else { None };

    // ── 1. Embedding search ──────────────────────────────────────────────────
    if let Some(vec) = crate::embedding::embedder::embed_text(client, embedding_url, query).await {
        #[derive(serde::Deserialize)]
        struct EmbRow { page_id: String, score: f32 }
        let mut resp = if let Some(sid) = kb_session_id {
            db.query(
                "SELECT page_id, vector::similarity::cosine(embedding, $vec) AS score \
                 FROM import_pages WHERE vault_id = $vid AND session_id = $sid \
                 AND status = 'imported' AND embedding IS NOT NONE \
                 ORDER BY score DESC LIMIT 8"
            )
            .bind(("vid", vault_id.to_string()))
            .bind(("sid", sid.to_string()))
            .bind(("vec", vec))
            .await.map_err(|e| e.to_string())?
        } else {
            db.query(
                "SELECT page_id, vector::similarity::cosine(embedding, $vec) AS score \
                 FROM import_pages WHERE vault_id = $vid AND status = 'imported' \
                 AND embedding IS NOT NONE ORDER BY score DESC LIMIT 8"
            )
            .bind(("vid", vault_id.to_string()))
            .bind(("vec", vec))
            .await.map_err(|e| e.to_string())?
        };
        let emb_rows: Vec<EmbRow> = resp.take(0).unwrap_or_default();

        if !emb_rows.is_empty() {
            let ids: Vec<String> = emb_rows.iter().map(|r| r.page_id.clone()).collect();
            let score_map: std::collections::HashMap<String, f32> =
                emb_rows.into_iter().map(|r| (r.page_id, r.score)).collect();
            let mut pr = db.query(
                "SELECT page_id, url, title, content_md, session_id, content_hash \
                 FROM import_pages WHERE page_id INSIDE $ids"
            )
            .bind(("ids", ids))
            .await.map_err(|e| e.to_string())?;
            let mut rows: Vec<PageRow> = pr.take(0).unwrap_or_default();
            rows.sort_by(|a, b| {
                let sa = score_map.get(&a.page_id).copied().unwrap_or(0.0);
                let sb = score_map.get(&b.page_id).copied().unwrap_or(0.0);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            });
            return Ok(build_kb_results(&rows, Some(&score_map)));
        }
    }

    // ── 2. FTS fallback: already imported ────────────────────────────────────
    let q_lower = format!("%{}%", query.to_lowercase());
    let mut imported: Vec<PageRow> = {
        let mut resp = if let Some(sid) = kb_session_id {
            db.query(
                "SELECT page_id, url, title, content_md, session_id, content_hash \
                 FROM import_pages WHERE vault_id = $vid AND session_id = $sid \
                 AND status = 'imported' \
                 AND (string::lowercase(title ?? '') CONTAINS $q \
                      OR string::lowercase(content_md ?? '') CONTAINS $q) \
                 ORDER BY last_crawled DESC LIMIT 8"
            )
            .bind(("vid", vault_id.to_string()))
            .bind(("sid", sid.to_string()))
            .bind(("q",   q_lower.clone()))
            .await.map_err(|e| e.to_string())?
        } else {
            db.query(
                "SELECT page_id, url, title, content_md, session_id, content_hash \
                 FROM import_pages WHERE vault_id = $vid AND status = 'imported' \
                 AND (string::lowercase(title ?? '') CONTAINS $q \
                      OR string::lowercase(content_md ?? '') CONTAINS $q) \
                 ORDER BY last_crawled DESC LIMIT 8"
            )
            .bind(("vid", vault_id.to_string()))
            .bind(("q",   q_lower.clone()))
            .await.map_err(|e| e.to_string())?
        };
        resp.take(0).unwrap_or_default()
    };

    // ── 3. On-demand fetch: pending pages matching by title ──────────────────
    if imported.is_empty() {
        let pending: Vec<PageRow> = {
            let mut resp = if let Some(sid) = kb_session_id {
                db.query(
                    "SELECT page_id, url, title, content_md, session_id, content_hash \
                     FROM import_pages WHERE vault_id = $vid AND session_id = $sid \
                     AND status = 'pending' AND string::lowercase(title ?? '') CONTAINS $q LIMIT 5"
                )
                .bind(("vid", vault_id.to_string()))
                .bind(("sid", sid.to_string()))
                .bind(("q",   q_lower.clone()))
                .await.map_err(|e| e.to_string())?
            } else {
                db.query(
                    "SELECT page_id, url, title, content_md, session_id, content_hash \
                     FROM import_pages WHERE vault_id = $vid AND status = 'pending' \
                     AND string::lowercase(title ?? '') CONTAINS $q LIMIT 5"
                )
                .bind(("vid", vault_id.to_string()))
                .bind(("q",   q_lower.clone()))
                .await.map_err(|e| e.to_string())?
            };
            resp.take(0).unwrap_or_default()
        };

        for page in pending {
            let result = async {
                let html = client
                    .get(&page.url)
                    .header("User-Agent", "noteTreeLM/1.0")
                    .timeout(std::time::Duration::from_secs(30))
                    .send().await.map_err(|e| e.to_string())?
                    .text().await.map_err(|e| e.to_string())?;
                let content_md = html_to_markdown(&html);
                let hash = sha256_hex(&content_md);
                let now_ms = chrono::Utc::now().timestamp_millis();
                // Embed on fetch
                let embedding = crate::embedding::embedder::embed_text(client, embedding_url, &content_md).await;
                let emb_val: Value = embedding.map(|v| json!(v)).unwrap_or(Value::Null);
                db.query(
                    "UPDATE import_pages SET status = 'imported', content_md = $cmd, \
                     content_hash = $hash, last_crawled = $now, embedding = $emb \
                     WHERE page_id = $pid AND vault_id = $vid"
                )
                .bind(("cmd",  content_md.clone()))
                .bind(("hash", hash))
                .bind(("now",  now_ms))
                .bind(("emb",  emb_val))
                .bind(("pid",  page.page_id.clone()))
                .bind(("vid",  vault_id.to_string()))
                .await.map_err(|e| e.to_string())?;
                Ok::<_, String>(content_md)
            }.await;

            match result {
                Ok(content_md) => {
                    let q_l = query.to_lowercase();
                    if content_md.to_lowercase().contains(&q_l)
                        || page.title.as_deref().unwrap_or("").to_lowercase().contains(&q_l)
                    {
                        imported.push(PageRow {
                            page_id:      page.page_id,
                            url:          page.url,
                            title:        page.title,
                            content_md:   Some(content_md),
                            session_id:   page.session_id,
                            content_hash: None,
                        });
                    }
                }
                Err(e) => tracing::warn!("[search_kb_pages] on-demand fetch failed: {}", e),
            }
        }
    }

    if imported.is_empty() { return Ok(json!([])); }
    Ok(build_kb_results(&imported, None))
}

fn build_kb_results(
    rows:      &[impl AsPageRow],
    score_map: Option<&std::collections::HashMap<String, f32>>,
) -> Value {
    let results: Vec<Value> = rows.iter().enumerate().map(|(i, p)| {
        let cite_id = format!("kb_{}", i + 1);
        let body = p.content_md().unwrap_or("");
        let body = if body.starts_with("---") {
            body.splitn(4, "---").nth(2).unwrap_or(body).trim_start()
        } else { body };
        let score = score_map.and_then(|m| m.get(p.page_id())).copied();
        let mut obj = json!({
            "__cite_id__": cite_id,
            "url":         p.url(),
            "title":       p.title().unwrap_or_else(|| p.url()),
            "content":     body.chars().take(1200).collect::<String>(),
        });
        if let Some(s) = score {
            obj["score"] = json!((s * 100.0).round() / 100.0);
        }
        obj
    }).collect();
    json!(results)
}

trait AsPageRow {
    fn page_id(&self) -> &str;
    fn url(&self) -> &str;
    fn title(&self) -> Option<&str>;
    fn content_md(&self) -> Option<&str>;
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
        tracing::warn!("[web_search] query={:?} → no API key configured", query);
        return Ok(json!("請至設定頁面設定 Brave Search API Key"));
    }

    let api_key = crate::service::harness::crypto::decrypt_api_key_db(db, &enc).await;
    if api_key.is_empty() {
        tracing::warn!("[web_search] query={:?} → API key decrypt failed", query);
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

    tracing::info!("[web_search] query={:?}", query);
    let results = brave_search_api(client, &api_key, query).await?;

    if results.is_empty() {
        tracing::info!("[web_search] query={:?} → 0 results", query);
        return Ok(json!(format!("Brave Search 未找到「{}」的搜尋結果。", query)));
    }
    tracing::info!("[web_search] query={:?} → {} results, titles={:?}",
        query, results.len(),
        results.iter().map(|(t, _, _)| t.as_str()).collect::<Vec<_>>(),
    );

    increment_brave_used(db, &key_id).await;

    // Emit web refs for frontend "儲存為知識" button + agent:refs for source chips
    let refs: Vec<Value> = results.iter().take(3).map(|(title, url, snippet)| {
        json!({ "path": url, "title": title, "excerpt": snippet })
    }).collect();
    state_emit("agent:web_refs", json!({
        "session_id": session_id,
        "refs": refs.clone(),
    }));
    state_emit("agent:refs", json!({
        "session_id": session_id,
        "refs": refs.iter().map(|r| {
            let mut v = r.clone();
            v["tool_name"] = json!("web_search");
            v
        }).collect::<Vec<_>>(),
    }));

    // Wrap each snippet individually — search result snippets come from arbitrary websites
    // and may contain prompt injection attempts.
    let formatted = results.iter().enumerate()
        .map(|(i, (title, url, snippet))| {
            let safe_snippet = crate::service::harness::security::wrap_external_content(
                &format!("Web:{}", url),
                snippet,
            );
            format!("[{}] **{}**\n{}\n來源：{}", i + 1, title, safe_snippet, url)
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

///// Extract source refs for agent:refs event.
/// Returns `[{ path, title?, excerpt?, tool_name }]`.
pub(crate) fn extract_note_refs(tool_name: &str, args: &Value, result: &Value, _vault_path: &str) -> Vec<Value> {
    match tool_name {
        "read_note" => {
            let p = args["path"].as_str().unwrap_or("");
            if p.is_empty() { return vec![]; }
            let lower = p.to_lowercase();
            let full = if lower.ends_with(".md") { lower } else { format!("{}.md", lower) };
            vec![json!({ "path": full, "tool_name": "read_note" })]
        }
        "open_note" => {
            let paths: Vec<String> = args["paths"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
                .unwrap_or_else(|| {
                    args["path"].as_str().map(|p| vec![p.to_string()]).unwrap_or_default()
                });
            paths.into_iter()
                .map(|p| json!({ "path": p, "tool_name": "open_note" }))
                .collect()
        }
        "search_vault" => {
            result.as_array()
                .map(|arr| arr.iter().filter_map(|v| {
                    let path = v["path"].as_str()?;
                    let mut obj = json!({ "path": path, "tool_name": "search_vault" });
                    if let Some(title) = v["title"].as_str() {
                        obj["title"] = json!(title);
                    }
                    Some(obj)
                }).collect())
                .unwrap_or_default()
        }
        _ => vec![],
    }
}

// ── fetch_url ─────────────────────────────────────────────────────────────────

/// Fetch a URL and return its text content, stripped of HTML tags.
/// Max 8000 chars to avoid blowing the context window.
pub(crate) async fn vault_fetch_url(
    client: &reqwest::Client,
    url: &str,
) -> Result<Value, String> {
    const MAX_CHARS: usize = 8000;

    let resp = client
        .get(url)
        .timeout(std::time::Duration::from_secs(15))
        .header("User-Agent", "Mozilla/5.0 (compatible; NoteTreeLM/1.0)")
        .send()
        .await
        .map_err(|e| format!("Fetch failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let content_type = resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let body = resp.text().await.map_err(|e| format!("Read failed: {}", e))?;

    // Strip HTML tags for HTML responses
    let text = if content_type.contains("html") {
        strip_html(&body)
    } else {
        body
    };

    let truncated: String = text.chars().take(MAX_CHARS).collect();
    let was_truncated = text.chars().count() > MAX_CHARS;

    Ok(json!({
        "url": url,
        "content": truncated,
        "truncated": was_truncated,
        "chars": truncated.len(),
    }))
}

fn strip_html(html: &str) -> String {
    // Remove <script> and <style> blocks entirely
    let mut s = html.to_string();
    for tag in &["script", "style"] {
        while let Some(start) = s.to_lowercase().find(&format!("<{}", tag)) {
            let end_tag = format!("</{}>", tag);
            if let Some(end) = s.to_lowercase()[start..].find(&end_tag) {
                s.drain(start..start + end + end_tag.len());
            } else {
                break;
            }
        }
    }
    // Strip remaining tags
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    // Collapse whitespace
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ── grep_vault ────────────────────────────────────────────────────────────────

/// Full-text string search across all markdown files in the vault.
/// Returns matching file paths + the line containing the match (first match per file).
/// Case-insensitive. Max 50 results.
pub(crate) fn vault_grep_notes(vault_path: &str, pattern: &str) -> Result<Value, String> {
    if vault_path.is_empty() { return Err("Vault 未設定".to_string()); }
    if pattern.is_empty() { return Err("pattern 不可為空".to_string()); }

    const MAX_RESULTS: usize = 50;
    let pattern_lower = pattern.to_lowercase();
    let base = std::path::Path::new(vault_path);
    let mut results: Vec<Value> = Vec::new();

    fn walk(
        dir: &std::path::Path,
        base: &std::path::Path,
        pattern: &str,
        results: &mut Vec<Value>,
        max: usize,
    ) {
        if results.len() >= max { return; }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if results.len() >= max { return; }
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.starts_with('.') { walk(&path, base, pattern, results, max); }
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let Ok(content) = std::fs::read_to_string(&path) else { continue };
                for (i, line) in content.lines().enumerate() {
                    if line.to_lowercase().contains(pattern) {
                        let rel = path.strip_prefix(base).unwrap_or(&path);
                        let snippet: String = line.trim().chars().take(120).collect();
                        results.push(json!({
                            "path": rel.to_string_lossy(),
                            "line": i + 1,
                            "snippet": snippet,
                        }));
                        break; // one match per file
                    }
                }
            }
        }
    }

    walk(base, base, &pattern_lower, &mut results, MAX_RESULTS);

    Ok(json!({
        "pattern": pattern,
        "count": results.len(),
        "results": results,
    }))
}

// ── batch_read_notes ──────────────────────────────────────────────────────────

/// Read multiple notes in one call. Each path is vault-relative.
/// Returns array of { path, content, error? }. Max 10 notes per call.
pub(crate) fn vault_batch_read_notes(vault_path: &str, paths: &[String]) -> Value {
    const MAX_NOTES: usize = 10;
    const MAX_CHARS_PER_NOTE: usize = 4000;

    let base = std::path::Path::new(vault_path);
    let results: Vec<Value> = paths.iter().take(MAX_NOTES).map(|rel_path| {
        if let Err(e) = validate_rel_path(rel_path) {
            return json!({ "path": rel_path, "error": e });
        }
        let full = base.join(rel_path);
        match std::fs::read_to_string(&full) {
            Ok(content) => {
                let truncated: String = content.chars().take(MAX_CHARS_PER_NOTE).collect();
                let was_truncated = content.chars().count() > MAX_CHARS_PER_NOTE;
                json!({ "path": rel_path, "content": truncated, "truncated": was_truncated })
            }
            Err(e) => json!({ "path": rel_path, "error": e.to_string() }),
        }
    }).collect();

    json!({ "notes": results, "count": results.len() })
}
