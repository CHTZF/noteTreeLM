//! Markdown Chunker — splits notes into heading-level chunks and
//! maintains the `chunks` table in SurrealDB.

use regex::Regex;
use sha2::{Digest, Sha256};
use serde::Deserialize;

use crate::db::surreal::SurrealDb;
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
    pub updated_at: i64,      // milliseconds
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

/// Upsert a set of chunks into the vault DB.
/// Deletes stale chunks from the same file that are no longer present.
/// If `embedding_url` is Some, calls the embedding server for each chunk and stores the vector.
pub async fn upsert_chunks(
    db: &SurrealDb,
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

    // Upsert each chunk (with optional embedding)
    let client = reqwest::Client::new();
    for c in chunks {
        let emb_vec = if let Some(url) = embedding_url {
            let v = crate::commands::ai::get_embedding(&client, url, &c.content).await;
            if v.is_empty() { None } else { Some(v) }
        } else {
            None
        };

        if let Some(vec) = emb_vec {
            let mut r = db.query(
                "INSERT INTO chunks (vault_id, chunk_id, file_path, section, content, links, chunk_type, word_count, updated_at, embedding, status)
                 VALUES ($vid, $cid, $fp, $section, $content, $links, $chunk_type, $wc, time::now(), $emb, $status)
                 ON DUPLICATE KEY UPDATE
                   content    = $content,
                   links      = $links,
                   word_count = $wc,
                   updated_at = time::now(),
                   embedding  = $emb,
                   status     = $status"
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
            let _ = r;
        } else {
            let mut r = db.query(
                "INSERT INTO chunks (vault_id, chunk_id, file_path, section, content, links, chunk_type, word_count, updated_at, status)
                 VALUES ($vid, $cid, $fp, $section, $content, $links, $chunk_type, $wc, time::now(), $status)
                 ON DUPLICATE KEY UPDATE
                   content    = $content,
                   links      = $links,
                   word_count = $wc,
                   updated_at = time::now(),
                   status     = $status"
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
            let _ = r;
        }
    }
    Ok(())
}

/// Update status for all chunks of a file (called after set_note_status).
pub async fn update_chunks_status(
    db: &SurrealDb,
    vault_id: &str,
    file_path: &str,
    status: &str,
) -> Result<(), AppError> {
    db.query("UPDATE chunks SET status = $status WHERE vault_id = $vid AND file_path = $fp")
        .bind(("status", status.to_owned()))
        .bind(("vid", vault_id.to_owned()))
        .bind(("fp", file_path.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// Remove all chunks for a given file path (called on note delete).
pub async fn delete_chunks(db: &SurrealDb, vault_id: &str, file_path: &str) -> Result<(), AppError> {
    db.query("DELETE FROM chunks WHERE vault_id = $vid AND file_path = $fp")
        .bind(("vid", vault_id.to_owned()))
        .bind(("fp", file_path.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
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
        upsert_chunks(db, vault_id, &chunks, embedding_url).await?;
    }
    Ok(count)
}
