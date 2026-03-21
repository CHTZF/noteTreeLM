//! Markdown Chunker — splits notes into heading-level chunks and
//! maintains the `chunks` table in SurrealDB.

use once_cell::sync::Lazy;
use regex::Regex;
use sha2::{Digest, Sha256};
use serde::Deserialize;

use crate::db::surreal::SurrealDb;
use crate::db::sqlite::SqliteConn;
use crate::error::AppError;

// ── Data types ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Chunk {
    pub id:         String,
    pub file_path:  String,
    pub section:    String,   // heading text, "" = preamble
    pub content:    String,   // raw Markdown of this section
    pub links:      Vec<String>, // wikilink target titles found in content
    pub chunk_type: String,
    pub word_count: i64,
    #[allow(dead_code)]
    pub updated_at: i64,      // milliseconds
    #[allow(dead_code)]
    pub embedding:  Option<Vec<f32>>,
    pub status:     String,   // from frontmatter: "draft" | "verified" | "deprecated" | ""
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Stable ID: first 16 hex chars of SHA-256(file_path "::" section)
fn chunk_id(file_path: &str, section: &str) -> String {
    let input = format!("{}::{}", file_path, section);
    let hash = Sha256::digest(input.as_bytes());
    format!("{:x}", hash)[..16].to_string()
}

/// Extract all [[wikilink]] target titles (ignores alias and heading suffix)
fn extract_wikilinks(text: &str) -> Vec<String> {
    let re = Regex::new(r"\[\[([^\]|#\n]+)(?:[|#][^\]]*)?]]").unwrap();
    re.captures_iter(text)
        .map(|c| c[1].trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn word_count(text: &str) -> i64 {
    text.split_whitespace().count() as i64
}

// ── Embedding pre-processing ───────────────────────────────────────────────

static RE_FRONTMATTER:  Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)^---\r?\n.*?\n---\r?\n?").unwrap());
static RE_CODE_BLOCK:   Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)```[^\n]*\n.*?```").unwrap());
static RE_INLINE_CODE:  Lazy<Regex> = Lazy::new(|| Regex::new(r"`[^`\n]+`").unwrap());
static RE_HTML_TAG:     Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").unwrap());
static RE_WIKILINK:     Lazy<Regex> = Lazy::new(|| Regex::new(r"\[\[([^\]|#]+)(?:[|#][^\]]*)?\]\]").unwrap());
static RE_MD_LINK:      Lazy<Regex> = Lazy::new(|| Regex::new(r"\[([^\]]*)\]\([^\)]*\)").unwrap());
static RE_HEADING:      Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^#{1,6}\s+").unwrap());
static RE_BOLD_ITALIC:  Lazy<Regex> = Lazy::new(|| Regex::new(r"\*{1,3}([^*\n]+)\*{1,3}").unwrap());
static RE_UNDERLINE:    Lazy<Regex> = Lazy::new(|| Regex::new(r"_{1,3}([^_\n]+)_{1,3}").unwrap());
static RE_BLOCKQUOTE:   Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^>\s?").unwrap());
static RE_LIST_MARKER:  Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^[-*+]\s+|^\d+\.\s+").unwrap());
static RE_HR:           Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^[-*_]{3,}\s*$").unwrap());
static RE_MULTI_SPACE:  Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\S\n]+").unwrap());
static RE_MULTI_NEWLINE:Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());

/// Clean chunk content before sending to the embedding server.
///
/// Removes Markdown syntax, HTML tags, and compresses whitespace.
/// **Does NOT strip punctuation** (punctuation carries semantic meaning in CJK).
/// The original `content` stored in DB is never modified — this is only applied
/// to the text passed to the embedding model.
pub fn clean_for_embedding(text: &str) -> String {
    // 1. Strip YAML frontmatter
    let s = RE_FRONTMATTER.replace_all(text, "");
    // 2. Remove fenced code blocks (keep a space so surrounding words don't merge)
    let s = RE_CODE_BLOCK.replace_all(&s, " ");
    // 3. Remove inline code
    let s = RE_INLINE_CODE.replace_all(&s, " ");
    // 4. Remove HTML tags
    let s = RE_HTML_TAG.replace_all(&s, " ");
    // 5. Decode common HTML entities
    let s = s
        .replace("&amp;",  "&")
        .replace("&lt;",   "<")
        .replace("&gt;",   ">")
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;",  "'");
    // 6. Wikilinks [[text|alias]] → text
    let s = RE_WIKILINK.replace_all(&s, "$1");
    // 7. Markdown links [text](url) → text
    let s = RE_MD_LINK.replace_all(&s, "$1");
    // 8. Heading markers (## Title → Title)
    let s = RE_HEADING.replace_all(&s, "");
    // 9. Bold / italic markers (**text** → text)
    let s = RE_BOLD_ITALIC.replace_all(&s, "$1");
    let s = RE_UNDERLINE.replace_all(&s, "$1");
    // 10. Blockquote markers
    let s = RE_BLOCKQUOTE.replace_all(&s, "");
    // 11. List markers (- item / 1. item)
    let s = RE_LIST_MARKER.replace_all(&s, "");
    // 12. Horizontal rules
    let s = RE_HR.replace_all(&s, "");
    // 13. Compress non-newline whitespace → single space
    let s = RE_MULTI_SPACE.replace_all(&s, " ");
    // 14. Compress 3+ consecutive newlines → 2
    let s = RE_MULTI_NEWLINE.replace_all(&s, "\n\n");

    s.trim().to_string()
}

/// Parse the `status:` field from YAML frontmatter (--- ... ---)
pub fn parse_frontmatter_status(content: &str) -> String {
    if !content.starts_with("---") { return String::new(); }
    let after = if content.starts_with("---\r\n") { 5 } else { 4 };
    let end = match content[after..].find("\n---") {
        Some(i) => after + i,
        None => return String::new(),
    };
    for line in content[after..end].lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("status:") {
            let val = trimmed["status:".len()..].trim();
            if matches!(val, "verified" | "draft" | "deprecated") {
                return val.to_string();
            }
        }
    }
    String::new()
}

// ── Public API ────────────────────────────────────────────────────────────

/// Split a Markdown note into heading-level chunks.
///
/// Strategy:
/// - Text before the first heading → chunk with section = ""
/// - Each heading starts a new chunk (content = from this heading until next
///   heading of same or higher level)
/// - Empty sections are omitted
pub fn chunk_note(file_path: &str, content: &str, now_ms: i64) -> Vec<Chunk> {
    let status = parse_frontmatter_status(content);
    let heading_re = Regex::new(r"(?m)^(#{1,6}) (.+)$").unwrap();

    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    let mut current_section = String::new(); // "" = preamble
    let mut current_lines: Vec<&str> = Vec::new();

    for line in content.lines() {
        if let Some(caps) = heading_re.captures(line) {
            let section_name = caps[2].trim().to_string();
            let body = current_lines.join("\n").trim().to_string();
            if !body.is_empty() {
                sections.push((current_section.clone(), current_lines.clone()));
            }
            current_section = section_name;
            current_lines = vec![line];
        } else {
            current_lines.push(line);
        }
    }
    // last section
    let body = current_lines.join("\n").trim().to_string();
    if !body.is_empty() {
        sections.push((current_section, current_lines));
    }

    sections
        .into_iter()
        .map(|(section, lines)| {
            let text = lines.join("\n").trim().to_string();
            let links = extract_wikilinks(&text);
            Chunk {
                id:         chunk_id(file_path, &section),
                file_path:  file_path.to_string(),
                section:    section,
                content:    text.clone(),
                links,
                chunk_type: "text".to_string(),
                word_count: word_count(&text),
                updated_at: now_ms,
                embedding:  None,
                status:     status.clone(),
            }
        })
        .collect()
}

/// Upsert a set of chunks into the vault DB (SurrealDB) and best-effort into
/// the SQLite FTS5 index. SQLite failures are logged and ignored.
/// Deletes stale chunks from the same file that are no longer present.
/// If `embedding_url` is Some, calls the embedding server for each chunk and stores the vector.
pub async fn upsert_chunks(
    db: &SurrealDb,
    sqlite: Option<&SqliteConn>,
    vault_id: &str,
    chunks: &[Chunk],
    embedding_url: Option<&str>,
) -> Result<(), AppError> {
    if chunks.is_empty() {
        return Ok(());
    }
    let file_path = &chunks[0].file_path;

    // Collect current chunk ids
    let ids: Vec<&str> = chunks.iter().map(|c| c.id.as_str()).collect();

    // Delete stale chunks (those for this file+vault not in the current set)
    #[derive(Deserialize)]
    struct ChunkIdRow { chunk_id: String }

    let mut resp = db
        .query("SELECT chunk_id FROM chunks WHERE vault_id = $vid AND file_path = $fp")
        .bind(("vid", vault_id.to_owned()))
        .bind(("fp", file_path.as_str().to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let existing_rows: Vec<ChunkIdRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    for row in existing_rows {
        if !ids.contains(&row.chunk_id.as_str()) {
            db.query("DELETE FROM chunks WHERE vault_id = $vid AND chunk_id = $cid")
                .bind(("vid", vault_id.to_owned()))
                .bind(("cid", row.chunk_id.clone()))
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
        }
    }

    // Embed all chunks (clean text before sending to embedding server)
    let client = reqwest::Client::new();
    let embeddings: Vec<Option<Vec<f32>>> = if let Some(url) = embedding_url {
        let url_str = url.to_string();
        let futs: Vec<_> = chunks.iter().map(|c| {
            let client = client.clone();
            let url = url_str.clone();
            let content = clean_for_embedding(&c.content);
            async move {
                let v = crate::commands::ai::get_embedding(&client, &url, &content).await;
                if v.is_empty() { None } else { Some(v) }
            }
        }).collect();
        futures::future::join_all(futs).await
    } else {
        vec![None; chunks.len()]
    };

    // Delete then insert each chunk.
    // Avoids ON DUPLICATE KEY UPDATE which triggers a SurrealDB FTS B-tree bug
    // (duplicate insert key panic) when the chunks table has a BM25 index.
    for (c, emb_vec) in chunks.iter().zip(embeddings.into_iter()) {
        db.query("DELETE FROM chunks WHERE vault_id = $vid AND chunk_id = $cid")
            .bind(("vid", vault_id.to_owned()))
            .bind(("cid", c.id.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

        if let Some(vec) = emb_vec {
            db.query(
                "INSERT INTO chunks (vault_id, chunk_id, file_path, section, content, links, chunk_type, word_count, updated_at, embedding, status)
                 VALUES ($vid, $cid, $fp, $section, $content, $links, $chunk_type, $wc, time::now(), $emb, $status)"
            )
            .bind(("vid", vault_id.to_owned()))
            .bind(("cid", c.id.clone()))
            .bind(("fp", c.file_path.clone()))
            .bind(("section", c.section.clone()))
            .bind(("content", c.content.clone()))
            .bind(("links", c.links.clone()))
            .bind(("chunk_type", c.chunk_type.clone()))
            .bind(("wc", c.word_count))
            .bind(("emb", vec))
            .bind(("status", c.status.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        } else {
            db.query(
                "INSERT INTO chunks (vault_id, chunk_id, file_path, section, content, links, chunk_type, word_count, updated_at, status)
                 VALUES ($vid, $cid, $fp, $section, $content, $links, $chunk_type, $wc, time::now(), $status)"
            )
            .bind(("vid", vault_id.to_owned()))
            .bind(("cid", c.id.clone()))
            .bind(("fp", c.file_path.clone()))
            .bind(("section", c.section.clone()))
            .bind(("content", c.content.clone()))
            .bind(("links", c.links.clone()))
            .bind(("chunk_type", c.chunk_type.clone()))
            .bind(("wc", c.word_count))
            .bind(("status", c.status.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        }
    }

    // Best-effort: upsert all chunks into SQLite FTS5.
    if let Some(sq) = sqlite {
        let sq = sq.clone();
        let fts_rows: Vec<(String, String, String, String, String)> = chunks.iter()
            .map(|c| (c.id.clone(), c.file_path.clone(), c.section.clone(), c.content.clone(), c.status.clone()))
            .collect();
        let vid = vault_id.to_owned();
        tokio::task::spawn_blocking(move || {
            if let Ok(conn) = sq.lock() {
                for (chunk_id, file_path, section, content, status) in &fts_rows {
                    if let Err(e) = crate::db::sqlite::fts_upsert(
                        &conn, chunk_id, &vid, file_path, section, content, status,
                    ) {
                        log::warn!("SQLite FTS upsert failed for {chunk_id}: {e}");
                    }
                }
            }
        }).await.ok();
    }

    Ok(())
}


/// Insert-only batch upsert（跳過 delete 步驟）。
/// 用於重建索引時：呼叫前已一次性清空 chunks，直接批次 INSERT。
/// - 每批 BATCH_SIZE 筆，一次 round-trip → 速度大幅提升
/// - 無 DELETE+INSERT 同 record，避免 FTS B-tree duplicate key panic
pub async fn insert_chunks_only(
    db: &SurrealDb,
    vault_id: &str,
    chunks: &[Chunk],
    embedding_url: Option<&str>,
) -> Result<(), AppError> {
    if chunks.is_empty() { return Ok(()); }

    const BATCH_SIZE: usize = 50;

    // 並行計算所有 embeddings（保持原有並行優化）
    let client = reqwest::Client::new();
    let embeddings: Vec<Option<Vec<f32>>> = if let Some(url) = embedding_url {
        let url_str = url.to_string();
        let futs: Vec<_> = chunks.iter().map(|c| {
            let client = client.clone();
            let url = url_str.clone();
            let content = c.content.clone();
            async move {
                let v = crate::commands::ai::get_embedding(&client, &url, &content).await;
                if v.is_empty() { None } else { Some(v) }
            }
        }).collect();
        futures::future::join_all(futs).await
    } else {
        vec![None; chunks.len()]
    };

    // 批次 INSERT：每 BATCH_SIZE 筆一次 round-trip
    for (batch_c, batch_e) in chunks.chunks(BATCH_SIZE).zip(embeddings.chunks(BATCH_SIZE)) {
        let records: Vec<serde_json::Value> = batch_c.iter().zip(batch_e.iter())
            .map(|(c, emb)| {
                let mut obj = serde_json::json!({
                    "vault_id":   vault_id,
                    "chunk_id":   c.id,
                    "file_path":  c.file_path,
                    "section":    c.section,
                    "content":    c.content,
                    "links":      c.links,
                    "chunk_type": c.chunk_type,
                    "word_count": c.word_count,
                    "status":     c.status,
                    // updated_at 由 schema DEFAULT time::now() 自動設定
                });
                if let Some(ref vec) = emb {
                    obj["embedding"] = serde_json::json!(vec);
                }
                obj
            }).collect();

        // Use .query() instead of .insert().content() to avoid
        // SurrealDB Thing (enum) serialization error when deserializing the response.
        db.query("INSERT INTO chunks $r")
            .bind(("r", records))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
    }
    Ok(())
}

/// 接受外部預算好的 embeddings，直接批次 INSERT。
/// 用於 reindex_vault_chunks：embedding 已在呼叫端並行計算完畢，這裡只做 DB 寫入。
pub async fn batch_insert_with_embeddings(
    db: &SurrealDb,
    vault_id: &str,
    chunks: &[Chunk],
    embeddings: &[Option<Vec<f32>>],
) -> Result<(), AppError> {
    if chunks.is_empty() { return Ok(()); }

    const BATCH_SIZE: usize = 50;

    for (batch_c, batch_e) in chunks.chunks(BATCH_SIZE).zip(embeddings.chunks(BATCH_SIZE)) {
        let records: Vec<serde_json::Value> = batch_c.iter().zip(batch_e.iter())
            .map(|(c, emb)| {
                let mut obj = serde_json::json!({
                    "vault_id":   vault_id,
                    "chunk_id":   c.id,
                    "file_path":  c.file_path,
                    "section":    c.section,
                    "content":    c.content,
                    "links":      c.links,
                    "chunk_type": c.chunk_type,
                    "word_count": c.word_count,
                    "status":     c.status,
                    // updated_at 由 schema DEFAULT time::now() 自動設定
                });
                if let Some(ref vec) = emb {
                    obj["embedding"] = serde_json::json!(vec);
                }
                obj
            }).collect();

        // Use .query() instead of .insert().content() to avoid
        // SurrealDB Thing (enum) serialization error when deserializing the response.
        db.query("INSERT INTO chunks $r")
            .bind(("r", records))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
    }
    Ok(())
}

/// Remove all chunks for a given file path (called on note delete).
/// Also removes from SQLite FTS5 best-effort.
pub async fn delete_chunks(
    db: &SurrealDb,
    sqlite: Option<&SqliteConn>,
    vault_id: &str,
    file_path: &str,
) -> Result<(), AppError> {
    db.query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
        .bind(("vid", vault_id.to_owned()))
        .bind(("fp", file_path.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    if let Some(sq) = sqlite {
        let sq = sq.clone();
        let vid = vault_id.to_owned();
        let fp = file_path.to_owned();
        tokio::task::spawn_blocking(move || {
            if let Ok(conn) = sq.lock() {
                if let Err(e) = crate::db::sqlite::fts_delete_file(&conn, &vid, &fp) {
                    log::warn!("SQLite FTS delete failed for {fp}: {e}");
                }
            }
        }).await.ok();
    }

    Ok(())
}

/// Bulk reindex: re-chunk every note in the DB.
/// Returns the number of notes processed.
/// Pass `embedding_url` to also embed each chunk during reindex.
pub async fn reindex_all(
    db: &SurrealDb,
    vault_id: &str,
    embedding_url: Option<&str>,
) -> Result<usize, AppError> {
    #[derive(Deserialize)]
    struct NotePathContent {
        path: String,
        content: String,
    }

    let mut resp = db
        .query("SELECT path, content FROM notes WHERE vault_id = $vid")
        .bind(("vid", vault_id.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let notes: Vec<NotePathContent> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    let count = notes.len();
    let now = chrono::Utc::now().timestamp_millis();

    for note in &notes {
        let chunks = chunk_note(&note.path, &note.content, now);
        upsert_chunks(db, None, vault_id, &chunks, embedding_url).await?;
    }
    Ok(count)
}
