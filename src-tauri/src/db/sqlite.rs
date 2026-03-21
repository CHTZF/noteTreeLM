//! SQLite FTS5 search index.
//!
//! Stores chunk content in a virtual FTS5 table with `trigram` tokenizer,
//! which supports both CJK and Latin-script full-text search.
//!
//! This is the authoritative source for full-text search.
//! SurrealDB `chunks` table remains the authoritative source for all other
//! chunk metadata (embedding vectors, links, word_count, …).
//!
//! **Dual-write strategy (Strategy A)**:
//! - All writes go to SurrealDB first.
//! - SQLite is updated best-effort after each SurrealDB write.
//! - On failure the error is logged and ignored; the caller should reindex.

use rusqlite::{params, Connection, Result as SqliteResult};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Shareable, Send+Sync handle to the SQLite connection.
pub type SqliteConn = Arc<Mutex<Connection>>;

// ── Init ──────────────────────────────────────────────────────────────────

/// Open (or create) the search database at `path` and ensure the FTS5
/// virtual table exists.
pub fn init_sqlite(path: &Path) -> SqliteResult<SqliteConn> {
    let conn = Connection::open(path)?;

    // WAL mode: better concurrent read performance
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;

    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts
         USING fts5(
             chunk_id  UNINDEXED,
             vault_id  UNINDEXED,
             file_path UNINDEXED,
             section   UNINDEXED,
             content,
             status    UNINDEXED,
             tokenize  = 'trigram'
         );",
    )?;

    Ok(Arc::new(Mutex::new(conn)))
}

// ── Write helpers ─────────────────────────────────────────────────────────

/// Upsert one chunk into the FTS5 index.
/// FTS5 has no native UPSERT, so we delete-then-insert.
pub fn fts_upsert(
    conn: &Connection,
    chunk_id: &str,
    vault_id: &str,
    file_path: &str,
    section: &str,
    content: &str,
    status: &str,
) -> SqliteResult<()> {
    conn.execute(
        "DELETE FROM chunks_fts WHERE chunk_id = ?1 AND vault_id = ?2",
        params![chunk_id, vault_id],
    )?;
    conn.execute(
        "INSERT INTO chunks_fts(chunk_id, vault_id, file_path, section, content, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![chunk_id, vault_id, file_path, section, content, status],
    )?;
    Ok(())
}

/// Delete all FTS rows for a given file (called on note delete / stale purge).
pub fn fts_delete_file(
    conn: &Connection,
    vault_id: &str,
    file_path: &str,
) -> SqliteResult<()> {
    conn.execute(
        "DELETE FROM chunks_fts WHERE vault_id = ?1 AND file_path = ?2",
        params![vault_id, file_path],
    )?;
    Ok(())
}

/// Rebuild the FTS index for a vault from a pre-computed list of chunks.
/// Clears existing rows for the vault first, then batch-inserts.
pub fn fts_rebuild_vault(
    conn: &Connection,
    vault_id: &str,
    chunks: &[(String, String, String, String, String)], // (chunk_id, file_path, section, content, status)
) -> SqliteResult<()> {
    conn.execute(
        "DELETE FROM chunks_fts WHERE vault_id = ?1",
        params![vault_id],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO chunks_fts(chunk_id, vault_id, file_path, section, content, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (chunk_id, file_path, section, content, status) in chunks {
        stmt.execute(params![chunk_id, vault_id, file_path, section, content, status])?;
    }
    Ok(())
}

// ── Search ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct FtsResult {
    pub chunk_id:  String,
    pub file_path: String,
    pub section:   String,
    pub content:   String,
    pub status:    String,
}

/// Full-text search over chunks for a vault.
///
/// The query is automatically escaped and wrapped in double quotes so it
/// becomes a trigram substring/phrase match (no FTS5 special-char issues).
pub fn fts_search(
    conn: &Connection,
    vault_id: &str,
    query: &str,
    verified_only: bool,
    limit: usize,
) -> SqliteResult<Vec<FtsResult>> {
    let escaped = escape_fts5(query);
    let sql = if verified_only {
        format!(
            "SELECT chunk_id, file_path, section, content, status
             FROM chunks_fts
             WHERE chunks_fts MATCH ?1 AND vault_id = ?2 AND status = 'verified'
             LIMIT {limit}"
        )
    } else {
        format!(
            "SELECT chunk_id, file_path, section, content, status
             FROM chunks_fts
             WHERE chunks_fts MATCH ?1 AND vault_id = ?2
             LIMIT {limit}"
        )
    };

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![escaped, vault_id], |row| {
        Ok(FtsResult {
            chunk_id:  row.get(0)?,
            file_path: row.get(1)?,
            section:   row.get(2)?,
            content:   row.get(3)?,
            status:    row.get(4)?,
        })
    })?;
    rows.collect()
}

/// Return the number of indexed chunks for a vault (used for health check).
pub fn fts_count(conn: &Connection, vault_id: &str) -> SqliteResult<i64> {
    conn.query_row(
        "SELECT count(*) FROM chunks_fts WHERE vault_id = ?1",
        params![vault_id],
        |row| row.get(0),
    )
}

// ── Internal ──────────────────────────────────────────────────────────────

/// Escape a raw user query for FTS5 phrase search.
/// Wraps the whole query in double quotes; internal `"` are doubled.
/// With the trigram tokenizer this becomes a substring match.
fn escape_fts5(q: &str) -> String {
    let inner = q.replace('"', "\"\"");
    format!("\"{inner}\"")
}
