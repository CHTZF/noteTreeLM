-- noteTreeLM — Initial Schema
-- Version: 1

PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA cache_size   = -20000;
PRAGMA foreign_keys = ON;
PRAGMA temp_store   = MEMORY;

-- ============================================================
-- Schema Version
-- ============================================================
CREATE TABLE IF NOT EXISTS schema_version (
  version    INTEGER PRIMARY KEY,
  applied_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
INSERT OR IGNORE INTO schema_version(version) VALUES (1);

-- ============================================================
-- Notes
-- ============================================================
CREATE TABLE IF NOT EXISTS notes (
  path        TEXT    PRIMARY KEY,
  title       TEXT    NOT NULL,
  content     TEXT    NOT NULL DEFAULT '',
  frontmatter TEXT,
  word_count  INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL,
  modified_at INTEGER NOT NULL,
  checksum    TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_notes_title    ON notes(title);
CREATE INDEX IF NOT EXISTS idx_notes_modified ON notes(modified_at DESC);
CREATE INDEX IF NOT EXISTS idx_notes_created  ON notes(created_at  DESC);

-- ============================================================
-- Links (wikilinks, url refs, image embeds)
-- ============================================================
CREATE TABLE IF NOT EXISTS links (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  source_path  TEXT    NOT NULL
                 REFERENCES notes(path)
                 ON DELETE CASCADE
                 ON UPDATE CASCADE,
  target_title TEXT    NOT NULL,
  target_path  TEXT
                 REFERENCES notes(path)
                 ON DELETE SET NULL
                 ON UPDATE CASCADE,
  link_type    TEXT    NOT NULL
                 CHECK(link_type IN ('wikilink','url_ref','image_embed')),
  raw_text     TEXT    NOT NULL,
  alias        TEXT,
  heading      TEXT,
  line_number  INTEGER NOT NULL DEFAULT 0,
  UNIQUE(source_path, line_number, raw_text)
);
CREATE INDEX IF NOT EXISTS idx_links_source       ON links(source_path);
CREATE INDEX IF NOT EXISTS idx_links_target_title ON links(target_title);
CREATE INDEX IF NOT EXISTS idx_links_target_path  ON links(target_path);
CREATE INDEX IF NOT EXISTS idx_links_unresolved   ON links(target_path) WHERE target_path IS NULL;

-- ============================================================
-- Tags
-- ============================================================
CREATE TABLE IF NOT EXISTS tags (
  note_path TEXT NOT NULL
              REFERENCES notes(path)
              ON DELETE CASCADE
              ON UPDATE CASCADE,
  tag       TEXT NOT NULL,
  PRIMARY KEY (note_path, tag)
);
CREATE INDEX IF NOT EXISTS idx_tags_tag       ON tags(tag);
CREATE INDEX IF NOT EXISTS idx_tags_note_path ON tags(note_path);

-- ============================================================
-- Graph Nodes
-- ============================================================
CREATE TABLE IF NOT EXISTS graph_nodes (
  id         TEXT PRIMARY KEY,
  node_type  TEXT NOT NULL
               CHECK(node_type IN ('note','url','image','topic')),
  label      TEXT NOT NULL,
  url        TEXT,
  file_path  TEXT,
  metadata   TEXT,
  created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_graph_nodes_type ON graph_nodes(node_type);

-- ============================================================
-- Graph Edges
-- ============================================================
CREATE TABLE IF NOT EXISTS graph_edges (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id TEXT    NOT NULL REFERENCES graph_nodes(id) ON DELETE CASCADE,
  target_id TEXT    NOT NULL REFERENCES graph_nodes(id) ON DELETE CASCADE,
  edge_type TEXT    NOT NULL
              CHECK(edge_type IN (
                'wikilink','url_ref','image_embed',
                'topic_member','source_import'
              )),
  weight    REAL    NOT NULL DEFAULT 1.0,
  UNIQUE(source_id, target_id, edge_type)
);
CREATE INDEX IF NOT EXISTS idx_edges_source ON graph_edges(source_id);
CREATE INDEX IF NOT EXISTS idx_edges_target ON graph_edges(target_id);
CREATE INDEX IF NOT EXISTS idx_edges_type   ON graph_edges(edge_type);

-- ============================================================
-- Topics
-- ============================================================
CREATE TABLE IF NOT EXISTS topics (
  id         TEXT    PRIMARY KEY,
  name       TEXT    NOT NULL,
  description TEXT,
  keywords   TEXT,
  note_path  TEXT
               REFERENCES notes(path)
               ON DELETE SET NULL
               ON UPDATE CASCADE,
  created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
  updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE TABLE IF NOT EXISTS topic_memberships (
  topic_id  TEXT NOT NULL REFERENCES topics(id)    ON DELETE CASCADE,
  note_path TEXT NOT NULL REFERENCES notes(path)
              ON DELETE CASCADE
              ON UPDATE CASCADE,
  score     REAL NOT NULL DEFAULT 1.0,
  PRIMARY KEY (topic_id, note_path)
);
CREATE INDEX IF NOT EXISTS idx_topic_memberships_note ON topic_memberships(note_path);

-- ============================================================
-- Import History (duplicate URL detection)
-- ============================================================
CREATE TABLE IF NOT EXISTS imports (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  source_url    TEXT    NOT NULL UNIQUE,
  note_path     TEXT    REFERENCES notes(path)
                  ON DELETE SET NULL
                  ON UPDATE CASCADE,
  imported_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
  status        TEXT    NOT NULL
                  CHECK(status IN ('success','failed','partial')),
  error_message TEXT
);
CREATE INDEX IF NOT EXISTS idx_imports_url    ON imports(source_url);
CREATE INDEX IF NOT EXISTS idx_imports_status ON imports(status);

-- ============================================================
-- Assets (images in assets/ folder)
-- ============================================================
CREATE TABLE IF NOT EXISTS assets (
  file_path  TEXT PRIMARY KEY,
  mime_type  TEXT NOT NULL,
  file_size  INTEGER NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

-- ============================================================
-- FTS5 Full-Text Search
-- ============================================================
CREATE VIRTUAL TABLE IF NOT EXISTS search_fts USING fts5(
  path      UNINDEXED,
  title,
  content,
  tags,
  tokenize = 'unicode61 remove_diacritics 1'
);

CREATE TRIGGER IF NOT EXISTS notes_fts_insert AFTER INSERT ON notes BEGIN
  INSERT INTO search_fts(path, title, content)
    VALUES (new.path, new.title, new.content);
END;

CREATE TRIGGER IF NOT EXISTS notes_fts_update AFTER UPDATE OF title, content ON notes BEGIN
  UPDATE search_fts
    SET title = new.title, content = new.content
    WHERE path = new.path;
END;

CREATE TRIGGER IF NOT EXISTS notes_fts_delete AFTER DELETE ON notes BEGIN
  DELETE FROM search_fts WHERE path = old.path;
END;

-- ============================================================
-- Settings
-- ============================================================
CREATE TABLE IF NOT EXISTS settings (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

-- ============================================================
-- Trash Items (soft-delete bin)
-- ============================================================
CREATE TABLE IF NOT EXISTS trash_items (
  id             TEXT    PRIMARY KEY,
  original_path  TEXT    NOT NULL,
  name           TEXT    NOT NULL,
  title          TEXT    NOT NULL DEFAULT '',
  trash_filename TEXT    NOT NULL,
  deleted_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_trash_deleted ON trash_items(deleted_at DESC);

-- Default settings
INSERT OR IGNORE INTO settings(key, value) VALUES
  ('vault_path',         ''),
  ('theme',              'dark'),
  ('auto_save_mode',     'afterDelay'),
  ('auto_save_delay',    '1000'),
  ('whisper_cli_path',   ''),
  ('whisper_model_path', ''),
  ('whisper_language',   'auto'),
  ('whisper_auto_insert','true'),
  ('import_max_depth',   '3'),
  ('import_max_pages',   '50'),
  ('ai_provider',        ''),
  ('ai_model',           'gpt-4o'),
  ('ai_base_url',        'https://api.openai.com/v1'),
  ('ai_enable_topics',   'true'),
  ('ai_enable_summary',  'true'),
  ('ai_enable_vision',   'true'),
  ('last_open_note',     ''),
  ('window_width',       '1400'),
  ('window_height',      '900'),
  ('sidebar_width',      '240'),
  ('graph_panel_width',  '320'),
  ('onboarding_done',    'false'),
  ('recent_vaults',      '[]'),
  ('sort_orders',        '{}'),
  ('font_sans',          ''),
  ('font_mono',          ''),
  ('editor_font_size',   '14'),
  ('ui_font_size',       '14'),
  ('graph_font_size',    '11');
