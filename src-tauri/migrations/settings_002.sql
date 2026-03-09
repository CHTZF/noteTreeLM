-- noteTreeLM — Settings Schema v2
-- Adds: conversations, pending_plans

-- ============================================================
-- Conversations
-- ============================================================
CREATE TABLE IF NOT EXISTS conversations (
  id          TEXT PRIMARY KEY,
  account_id  TEXT,                            -- NULL until account system
  mode        TEXT NOT NULL DEFAULT 'chat',    -- 'chat' | 'live_chat'
  title       TEXT NOT NULL DEFAULT '',
  messages_json TEXT NOT NULL DEFAULT '[]',
  created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
  updated_at  INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE INDEX IF NOT EXISTS idx_conversations_mode_updated
  ON conversations(mode, updated_at DESC);

-- ============================================================
-- Pending Plans (per-conversation deferred tool execution)
-- ============================================================
CREATE TABLE IF NOT EXISTS pending_plans (
  conversation_id   TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
  deferred_tools_json TEXT NOT NULL DEFAULT '[]',   -- Vec<{name, args}> as JSON
  confirm_centroid  BLOB NOT NULL DEFAULT '',        -- Vec<f32> as LE bytes
  cancel_centroid   BLOB NOT NULL DEFAULT '',
  interrupt_centroid BLOB NOT NULL DEFAULT '',
  created_at        INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
