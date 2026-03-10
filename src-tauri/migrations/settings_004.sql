-- noteTreeLM — Per-user settings & accounts
-- Version: 4

-- ============================================================
-- Per-user settings (overrides global settings per user)
-- ============================================================
CREATE TABLE IF NOT EXISTS user_settings (
  username   TEXT NOT NULL,
  key        TEXT NOT NULL,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
  PRIMARY KEY (username, key)
);

-- ============================================================
-- Add user/user account (SHA-256 of "user")
-- ============================================================
INSERT OR IGNORE INTO users(username, password_hash)
  VALUES ('user', '04f8996da763b7a969b1028ee3007569eaf3a635486ddab211d512c85b9df8fb');

INSERT OR IGNORE INTO schema_version(version) VALUES (4);
