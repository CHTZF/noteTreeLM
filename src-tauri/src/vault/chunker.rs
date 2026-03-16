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

// ── Public API ────────────────────────────────────────────────────────────

/// Split a Markdown note into heading-level chunks.
///
/// Strategy:
/// - Text before the first heading → chunk with section = ""
/// - Each heading starts a new chunk (content = from this heading until next
///   heading of same or higher level)
/// - Empty sections are omitted
pub fn chunk_note(file_path: &str, content: &str, now_ms: i64) -> Vec<Chunk> {
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
            }
        })
        .collect()
}

/// Upsert a set of chunks into the vault DB.
/// Deletes stale chunks from the same file that are no longer present.
pub async fn upsert_chunks(db: &SurrealDb, vault_id: &str, chunks: &[Chunk]) -> Result<(), AppError> {
    if chunks.is_empty() {
        return Ok(());
    }
    let file_path = &chunks[0].file_path;

    // Collect current chunk ids
    let ids: Vec<&str> = chunks.iter().map(|c| c.id.as_str()).collect();

    // Delete stale chunks (those for this file+vault not in the current set)
    // We do this by fetching existing chunk_ids and deleting those not in ids
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

    // Upsert each chunk
    for c in chunks {
        db.query(
            "INSERT INTO chunks (vault_id, chunk_id, file_path, section, content, links, chunk_type, word_count, updated_at)
             VALUES ($vid, $cid, $fp, $section, $content, $links, $chunk_type, $wc, $updated_at)
             ON DUPLICATE KEY UPDATE
               content    = $content,
               links      = $links,
               word_count = $wc,
               updated_at = $updated_at"
        )
        .bind(("vid", vault_id.to_owned()))
        .bind(("cid", c.id.clone()))
        .bind(("fp", c.file_path.clone()))
        .bind(("section", c.section.clone()))
        .bind(("content", c.content.clone()))
        .bind(("links", c.links.clone()))
        .bind(("chunk_type", c.chunk_type.clone()))
        .bind(("wc", c.word_count))
        .bind(("updated_at", c.updated_at))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    }
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
pub async fn reindex_all(db: &SurrealDb, vault_id: &str) -> Result<usize, AppError> {
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
        upsert_chunks(db, vault_id, &chunks).await?;
    }
    Ok(count)
}
