//! Markdown Chunker — splits notes into heading-level chunks and
//! maintains the `chunks` / `chunks_fts` tables in the vault DB.

use regex::Regex;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

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
pub async fn upsert_chunks(db: &SqlitePool, chunks: &[Chunk]) -> Result<(), sqlx::Error> {
    if chunks.is_empty() {
        return Ok(());
    }
    let file_path = &chunks[0].file_path;

    // Delete chunks no longer present in the re-indexed note
    let ids: Vec<&str> = chunks.iter().map(|c| c.id.as_str()).collect();
    // Build placeholder list: ?,?,?...
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let delete_sql = format!(
        "DELETE FROM chunks WHERE file_path = ? AND id NOT IN ({})",
        placeholders
    );
    let mut q = sqlx::query(&delete_sql).bind(file_path);
    for id in &ids {
        q = q.bind(id);
    }
    q.execute(db).await?;

    // Upsert each chunk
    for c in chunks {
        let links_json = serde_json::to_string(&c.links).unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            "INSERT INTO chunks(id, file_path, section, content, links, chunk_type, word_count, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               content    = excluded.content,
               links      = excluded.links,
               word_count = excluded.word_count,
               updated_at = excluded.updated_at",
        )
        .bind(&c.id)
        .bind(&c.file_path)
        .bind(&c.section)
        .bind(&c.content)
        .bind(&links_json)
        .bind(&c.chunk_type)
        .bind(c.word_count)
        .bind(c.updated_at)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Remove all chunks for a given file path (called on note delete).
pub async fn delete_chunks(db: &SqlitePool, file_path: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM chunks WHERE file_path = ?")
        .bind(file_path)
        .execute(db)
        .await?;
    Ok(())
}

/// Bulk reindex: re-chunk every note in the DB.
/// Returns the number of notes processed.
pub async fn reindex_all(db: &SqlitePool) -> Result<usize, sqlx::Error> {
    let notes: Vec<(String, String)> =
        sqlx::query_as("SELECT path, content FROM notes")
            .fetch_all(db)
            .await?;

    let count = notes.len();
    let now = chrono::Utc::now().timestamp_millis();

    for (path, content) in &notes {
        let chunks = chunk_note(path, content, now);
        upsert_chunks(db, &chunks).await?;
    }
    Ok(count)
}
