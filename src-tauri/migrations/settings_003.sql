-- noteTreeLM — Auth Schema
-- Version: 3

-- ============================================================
-- Users table
-- ============================================================
CREATE TABLE IF NOT EXISTS users (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  username     TEXT UNIQUE NOT NULL,
  password_hash TEXT NOT NULL,  -- SHA-256 hex of password
  created_at   INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

-- Default admin/admin (SHA-256 of "admin")
INSERT OR IGNORE INTO users(username, password_hash)
  VALUES ('admin', '8c6976e5b5410415bde908bd4dee15dfb167a9c873fc4bb8a81f6f2ab448a918');

INSERT OR IGNORE INTO schema_version(version) VALUES (3);
