-- noteTreeLM — Vault Schema v2: Chunking + Graph-Aware Search
-- Each note is split into heading-level chunks for granular retrieval.

-- ── Chunks table ──────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS chunks (
    id          TEXT    PRIMARY KEY,        -- sha256(file_path||"::"||section)[0..16]
    file_path   TEXT    NOT NULL,           -- relative path, e.g. "folder/note.md"
    section     TEXT    NOT NULL DEFAULT '',-- heading text, "" = preamble before first heading
    content     TEXT    NOT NULL,           -- section text (raw Markdown)
    links       TEXT    NOT NULL DEFAULT '[]', -- JSON array of wikilink target titles
    chunk_type  TEXT    NOT NULL DEFAULT 'text',
    word_count  INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL,           -- milliseconds
    FOREIGN KEY(file_path) REFERENCES notes(path) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_path);

-- ── FTS5 index over chunk content ─────────────────────────────────────────
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    content,
    content      = 'chunks',
    content_rowid = 'rowid',
    tokenize     = 'unicode61 remove_diacritics 1'
);

-- Keep FTS in sync with chunks table
CREATE TRIGGER IF NOT EXISTS chunks_fts_insert
    AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TRIGGER IF NOT EXISTS chunks_fts_update
    AFTER UPDATE OF content ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
    INSERT INTO chunks_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TRIGGER IF NOT EXISTS chunks_fts_delete
    AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
END;
