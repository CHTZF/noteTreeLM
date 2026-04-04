use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use futures_util::StreamExt;
use serde_json::{json, Value};
use crate::api_state::ApiState;
use crate::db::SurrealDb;
use chrono::Datelike;
use crate::service_agent::harness::governance::guard::validate_rel_path;

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

    let api_key = crate::crypto::decrypt_api_key_db(db, &enc).await;
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

/// Validate citation ids from [cite:id1,id2] against the working memory evidence store.
/// Returns true if all ids are known (or working_memory is None = no validation needed).
async fn validate_citation(
    cite_inner: &str,
    working_memory: Option<&crate::service_agent::harness::memory::working::WorkingMemory>,
) -> bool {
    let wm = match working_memory {
        Some(w) => w,
        None => return true,
    };
    if cite_inner.trim() == "none" { return true; }
    let ids: Vec<String> = cite_inner.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if ids.is_empty() { return true; }
    wm.with_records(|map| ids.iter().all(|id| map.contains_key(id.as_str()))).await
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
    working_memory: Option<&crate::service_agent::harness::memory::working::WorkingMemory>,
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
                                                    let _valid = validate_citation(cite_inner, working_memory).await;
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
                                                if !cite_retry_done && working_memory.is_some() {
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
