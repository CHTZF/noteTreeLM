-- noteTreeLM — Settings Schema (Account-level, shared across vaults)
-- Version: 1

PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA cache_size   = -4000;
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
-- Settings (account-level, not vault-specific)
-- ============================================================
CREATE TABLE IF NOT EXISTS settings (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

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
  ('llm_model_path',     ''),
  ('llama_cli_path',     ''),
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
  ('graph_font_size',    '11'),
  ('debug_mode',         'false'),
  ('voice_process_mode', 'none'),
  ('enable_chat',        'false'),
  ('llama_server_port',  '8080'),
  ('whisper_server_port','8081'),
  ('enable_auto_memory', 'false'),
  ('memory_threshold',   '20');

-- ============================================================
-- Intent Keywords (for runtime intent classifier)
-- ============================================================
CREATE TABLE IF NOT EXISTS intent_keywords (
  intent   TEXT PRIMARY KEY,
  keywords TEXT NOT NULL  -- JSON array of strings
);

INSERT OR IGNORE INTO intent_keywords(intent, keywords) VALUES
  ('CANCEL',    '["算了","取消","不要","停止","不用了","停","不用","別","不行"]'),
  ('INTERRUPT', '["等等","先停","暫停","先等","hold on","wait"]'),
  ('CONFIRM',   '["確認","好的","是","對","OK","ok","確定","沒錯","對對","行","好"]'),
  ('REPEAT',    '["再說一次","再講一次","重複","請再說","再說","重說"]');

-- ============================================================
-- Vault States (per-vault metadata, e.g. last open note)
-- ============================================================
CREATE TABLE IF NOT EXISTS vault_states (
  vault_path     TEXT PRIMARY KEY,
  last_open_note TEXT NOT NULL DEFAULT '',
  updated_at     INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
