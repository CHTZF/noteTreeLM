use std::path::Path;
use std::sync::Arc;

use surrealdb::{
    Surreal,
    engine::local::{Db, SurrealKv},
};

pub type SurrealDb = Arc<Surreal<Db>>;

/// Schema 版本：每次修改 apply_schema 或 insert_default_settings 時遞增，
/// 確保已安裝的客戶端會重新執行一次 DDL，之後快取跳過。
const SCHEMA_VERSION: u32 = 1;

/// 初始化 SurrealDB（embedded SurrealKV）
/// 所有資料存於 app_data_dir/surrealdb/
pub async fn init_db(app_data_dir: &Path) -> crate::error::Result<SurrealDb> {
    let db_path = app_data_dir.join("surrealdb");
    tokio::fs::create_dir_all(&db_path).await?;

    let db = Surreal::new::<SurrealKv>(db_path.to_string_lossy().as_ref())
        .await
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;

    db.use_ns("notetreelm")
        .use_db("main")
        .await
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;

    // 版本快取：若版本檔匹配則跳過 ~100 條 DDL，大幅縮短啟動時間
    let version_file = app_data_dir.join("schema_version");
    let cached = tokio::fs::read_to_string(&version_file).await
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());

    if cached != Some(SCHEMA_VERSION) {
        apply_schema(&db).await?;
        let _ = tokio::fs::write(&version_file, SCHEMA_VERSION.to_string()).await;
    } else {
        // DDL 已快取，仍需插入可能新增的預設設定
        insert_default_settings(&db).await?;
    }

    Ok(Arc::new(db))
}

/// 定義所有 table / index
/// 使用 IF NOT EXISTS 語義（SurrealDB DEFINE … OVERWRITE 僅在需要更新時用）
async fn apply_schema(db: &Surreal<Db>) -> crate::error::Result<()> {
    let stmts: &[&str] = &[ // joined into one batch query below
        // ── 帳號層 ──────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS users SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS username   ON users TYPE string;",
        "DEFINE FIELD IF NOT EXISTS password_hash ON users TYPE string;",
        "DEFINE FIELD IF NOT EXISTS created_at ON users TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_users_username ON users FIELDS username UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS sessions SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS token      ON sessions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS username   ON sessions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS created_at ON sessions TYPE datetime DEFAULT time::now();",
        "DEFINE FIELD IF NOT EXISTS expires_at ON sessions TYPE datetime;",
        "DEFINE INDEX IF NOT EXISTS idx_sessions_token    ON sessions FIELDS token UNIQUE;",
        "DEFINE INDEX IF NOT EXISTS idx_sessions_username ON sessions FIELDS username;",

        // KV store for global settings
        "DEFINE TABLE IF NOT EXISTS settings SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS key        ON settings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS value      ON settings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS updated_at ON settings TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_settings_key ON settings FIELDS key UNIQUE;",

        // KV store for per-user settings
        "DEFINE TABLE IF NOT EXISTS user_settings SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS username   ON user_settings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS key        ON user_settings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS value      ON user_settings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS updated_at ON user_settings TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_user_settings_uk ON user_settings FIELDS username, key UNIQUE;",

        // Per-vault last open note
        "DEFINE TABLE IF NOT EXISTS vault_states SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_path     ON vault_states TYPE string;",
        "DEFINE FIELD IF NOT EXISTS last_open_note ON vault_states TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS updated_at     ON vault_states TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_vault_states_path ON vault_states FIELDS vault_path UNIQUE;",

        // ── 對話層 ───────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS conversations SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS id            ON conversations TYPE string;",
        "DEFINE FIELD IF NOT EXISTS account_id    ON conversations TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS vault_id      ON conversations TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS mode          ON conversations TYPE string DEFAULT 'chat';",
        "DEFINE FIELD IF NOT EXISTS title         ON conversations TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS messages_json ON conversations TYPE string DEFAULT '[]';",
        "DEFINE FIELD IF NOT EXISTS created_at    ON conversations TYPE datetime DEFAULT time::now();",
        "DEFINE FIELD IF NOT EXISTS updated_at    ON conversations TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_conv_mode_updated  ON conversations FIELDS mode, updated_at;",
        "DEFINE INDEX IF NOT EXISTS idx_conv_vault         ON conversations FIELDS vault_id;",

        "DEFINE TABLE IF NOT EXISTS pending_plans SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS conversation_id      ON pending_plans TYPE string;",
        "DEFINE FIELD IF NOT EXISTS deferred_tools_json  ON pending_plans TYPE string DEFAULT '[]';",
        "DEFINE FIELD IF NOT EXISTS confirm_centroid     ON pending_plans TYPE array<float> DEFAULT [];",
        "DEFINE FIELD IF NOT EXISTS cancel_centroid      ON pending_plans TYPE array<float> DEFAULT [];",
        "DEFINE FIELD IF NOT EXISTS interrupt_centroid   ON pending_plans TYPE array<float> DEFAULT [];",
        "DEFINE FIELD IF NOT EXISTS created_at           ON pending_plans TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_pending_plans_conv ON pending_plans FIELDS conversation_id UNIQUE;",

        // ── Vault 層（含 vault_id 欄位） ────────────────────────────
        "DEFINE TABLE IF NOT EXISTS notes SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id    ON notes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS path        ON notes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS title       ON notes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS content     ON notes TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS frontmatter ON notes TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS word_count  ON notes TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS created_at  ON notes TYPE datetime;",
        "DEFINE FIELD IF NOT EXISTS modified_at ON notes TYPE datetime;",
        "DEFINE FIELD IF NOT EXISTS checksum    ON notes TYPE string DEFAULT '';",
        "DEFINE INDEX IF NOT EXISTS idx_notes_vault_path ON notes FIELDS vault_id, path UNIQUE;",
        "DEFINE INDEX IF NOT EXISTS idx_notes_vault_modified ON notes FIELDS vault_id, modified_at;",
        "DEFINE INDEX IF NOT EXISTS idx_notes_vault_title ON notes FIELDS vault_id, title;",
        // BM25 Full-text search on notes
        "DEFINE ANALYZER IF NOT EXISTS note_analyzer TOKENIZERS blank,class FILTERS ascii,lowercase,ngram(1,10);",
        "DEFINE INDEX IF NOT EXISTS ft_notes ON notes FIELDS title, content SEARCH ANALYZER note_analyzer BM25 HIGHLIGHTS;",

        "DEFINE TABLE IF NOT EXISTS links SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id     ON links TYPE string;",
        "DEFINE FIELD IF NOT EXISTS source_path  ON links TYPE string;",
        "DEFINE FIELD IF NOT EXISTS target_title ON links TYPE string;",
        "DEFINE FIELD IF NOT EXISTS target_path  ON links TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS link_type    ON links TYPE string;",
        "DEFINE FIELD IF NOT EXISTS raw_text     ON links TYPE string;",
        "DEFINE FIELD IF NOT EXISTS alias        ON links TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS heading      ON links TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS line_number  ON links TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_links_source  ON links FIELDS vault_id, source_path;",
        "DEFINE INDEX IF NOT EXISTS idx_links_target  ON links FIELDS vault_id, target_path;",
        "DEFINE INDEX IF NOT EXISTS idx_links_uniq    ON links FIELDS vault_id, source_path, line_number, raw_text UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS tags SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id  ON tags TYPE string;",
        "DEFINE FIELD IF NOT EXISTS note_path ON tags TYPE string;",
        "DEFINE FIELD IF NOT EXISTS tag       ON tags TYPE string;",
        "DEFINE INDEX IF NOT EXISTS idx_tags_tag       ON tags FIELDS vault_id, tag;",
        "DEFINE INDEX IF NOT EXISTS idx_tags_note_uniq ON tags FIELDS vault_id, note_path, tag UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS graph_nodes SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id   ON graph_nodes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS node_id    ON graph_nodes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS node_type  ON graph_nodes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS label      ON graph_nodes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS url        ON graph_nodes TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS file_path  ON graph_nodes TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS metadata   ON graph_nodes TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS created_at ON graph_nodes TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_graph_nodes_vault_id ON graph_nodes FIELDS vault_id, node_id UNIQUE;",
        "DEFINE INDEX IF NOT EXISTS idx_graph_nodes_type     ON graph_nodes FIELDS vault_id, node_type;",

        "DEFINE TABLE IF NOT EXISTS graph_edges SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id  ON graph_edges TYPE string;",
        "DEFINE FIELD IF NOT EXISTS source_id ON graph_edges TYPE string;",
        "DEFINE FIELD IF NOT EXISTS target_id ON graph_edges TYPE string;",
        "DEFINE FIELD IF NOT EXISTS edge_type ON graph_edges TYPE string;",
        "DEFINE FIELD IF NOT EXISTS weight    ON graph_edges TYPE float DEFAULT 1.0;",
        "DEFINE INDEX IF NOT EXISTS idx_edges_source ON graph_edges FIELDS vault_id, source_id;",
        "DEFINE INDEX IF NOT EXISTS idx_edges_target ON graph_edges FIELDS vault_id, target_id;",
        "DEFINE INDEX IF NOT EXISTS idx_edges_uniq   ON graph_edges FIELDS vault_id, source_id, target_id, edge_type UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS topics SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id    ON topics TYPE string;",
        "DEFINE FIELD IF NOT EXISTS topic_id    ON topics TYPE string;",
        "DEFINE FIELD IF NOT EXISTS name        ON topics TYPE string;",
        "DEFINE FIELD IF NOT EXISTS description ON topics TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS keywords    ON topics TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS note_path   ON topics TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS created_at  ON topics TYPE datetime DEFAULT time::now();",
        "DEFINE FIELD IF NOT EXISTS updated_at  ON topics TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_topics_vault_id ON topics FIELDS vault_id, topic_id UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS topic_memberships SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id  ON topic_memberships TYPE string;",
        "DEFINE FIELD IF NOT EXISTS topic_id  ON topic_memberships TYPE string;",
        "DEFINE FIELD IF NOT EXISTS note_path ON topic_memberships TYPE string;",
        "DEFINE FIELD IF NOT EXISTS score     ON topic_memberships TYPE float DEFAULT 1.0;",
        "DEFINE INDEX IF NOT EXISTS idx_topic_memberships_note  ON topic_memberships FIELDS vault_id, note_path;",
        "DEFINE INDEX IF NOT EXISTS idx_topic_memberships_uniq  ON topic_memberships FIELDS vault_id, topic_id, note_path UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS assets SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id   ON assets TYPE string;",
        "DEFINE FIELD IF NOT EXISTS file_path  ON assets TYPE string;",
        "DEFINE FIELD IF NOT EXISTS mime_type  ON assets TYPE string;",
        "DEFINE FIELD IF NOT EXISTS file_size  ON assets TYPE int;",
        "DEFINE FIELD IF NOT EXISTS created_at ON assets TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_assets_vault_path ON assets FIELDS vault_id, file_path UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS trash_items SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id       ON trash_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS item_id        ON trash_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS original_path  ON trash_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS name           ON trash_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS title          ON trash_items TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS trash_filename ON trash_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS deleted_at     ON trash_items TYPE datetime;",
        "DEFINE INDEX IF NOT EXISTS idx_trash_vault_id ON trash_items FIELDS vault_id, item_id UNIQUE;",
        "DEFINE INDEX IF NOT EXISTS idx_trash_deleted  ON trash_items FIELDS vault_id, deleted_at;",

        // Simple URL-import history
        "DEFINE TABLE IF NOT EXISTS imports SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id   ON imports TYPE string;",
        "DEFINE FIELD IF NOT EXISTS source_url ON imports TYPE string;",
        "DEFINE FIELD IF NOT EXISTS note_path  ON imports TYPE string;",
        "DEFINE FIELD IF NOT EXISTS status     ON imports TYPE string DEFAULT 'success';",
        "DEFINE FIELD IF NOT EXISTS created_at ON imports TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_imports_vault_url ON imports FIELDS vault_id, source_url UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS memory_rules SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id     ON memory_rules TYPE string;",
        "DEFINE FIELD IF NOT EXISTS pattern_type ON memory_rules TYPE string;",
        "DEFINE FIELD IF NOT EXISTS pattern      ON memory_rules TYPE string;",
        "DEFINE FIELD IF NOT EXISTS value        ON memory_rules TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS created_at   ON memory_rules TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_memory_rules_vault_pattern ON memory_rules FIELDS vault_id, pattern UNIQUE;",

        // ── Chunks（語意搜尋基礎） ───────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS chunks SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id   ON chunks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS chunk_id   ON chunks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS file_path  ON chunks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS section    ON chunks TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS content    ON chunks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS links      ON chunks TYPE array<string> DEFAULT [];",
        "DEFINE FIELD IF NOT EXISTS chunk_type ON chunks TYPE string DEFAULT 'text';",
        "DEFINE FIELD IF NOT EXISTS word_count ON chunks TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS updated_at ON chunks TYPE datetime;",
        "DEFINE FIELD IF NOT EXISTS status     ON chunks TYPE string DEFAULT '';",
        // embedding 欄位（Phase 3 時啟用 HNSW）
        "DEFINE FIELD IF NOT EXISTS embedding  ON chunks TYPE option<array<float>>;",
        "DEFINE INDEX IF NOT EXISTS idx_chunks_vault_file ON chunks FIELDS vault_id, file_path;",
        "DEFINE INDEX IF NOT EXISTS idx_chunks_vault_id   ON chunks FIELDS vault_id, chunk_id UNIQUE;",
        // BM25 FTS on chunks
        "DEFINE INDEX IF NOT EXISTS ft_chunks ON chunks FIELDS content SEARCH ANALYZER note_analyzer BM25;",
        // HNSW vector index（Phase 3：啟用時需指定 DIMENSION N，目前略過）

        // ── Import Sessions（知識點匯入） ───────────────────────────
        "DEFINE TABLE IF NOT EXISTS import_sessions SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id       ON import_sessions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS session_id     ON import_sessions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS conversation_id ON import_sessions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS seed_url       ON import_sessions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS site_name      ON import_sessions TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS root_folder    ON import_sessions TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS site_outline   ON import_sessions TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS crawl_policy   ON import_sessions TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS status         ON import_sessions TYPE string DEFAULT 'active';",
        "DEFINE FIELD IF NOT EXISTS auto_update    ON import_sessions TYPE bool DEFAULT false;",
        "DEFINE FIELD IF NOT EXISTS created_at     ON import_sessions TYPE datetime DEFAULT time::now();",
        "DEFINE FIELD IF NOT EXISTS updated_at     ON import_sessions TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_import_sessions_vault_id ON import_sessions FIELDS vault_id, session_id UNIQUE;",
        "DEFINE INDEX IF NOT EXISTS idx_import_sessions_conv     ON import_sessions FIELDS conversation_id;",

        "DEFINE TABLE IF NOT EXISTS import_pages SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS vault_id      ON import_pages TYPE string;",
        "DEFINE FIELD IF NOT EXISTS page_id       ON import_pages TYPE string;",
        "DEFINE FIELD IF NOT EXISTS session_id    ON import_pages TYPE string;",
        "DEFINE FIELD IF NOT EXISTS url           ON import_pages TYPE string;",
        "DEFINE FIELD IF NOT EXISTS title         ON import_pages TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS parent_url    ON import_pages TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS depth         ON import_pages TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS note_path     ON import_pages TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS content_md   ON import_pages TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS content_hash  ON import_pages TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS http_etag     ON import_pages TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS status        ON import_pages TYPE string DEFAULT 'pending';",
        "DEFINE FIELD IF NOT EXISTS last_crawled  ON import_pages TYPE option<datetime>;",
        "DEFINE FIELD IF NOT EXISTS created_at    ON import_pages TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_import_pages_session ON import_pages FIELDS vault_id, session_id;",
        // OVERWRITE: changed from (vault_id, url) to (vault_id, session_id, url) so each session
        // can hold its own copy of a URL without conflicting with other sessions.
        "DEFINE INDEX OVERWRITE idx_import_pages_url ON import_pages FIELDS vault_id, session_id, url UNIQUE;",
        "DEFINE INDEX IF NOT EXISTS idx_import_pages_id      ON import_pages FIELDS vault_id, page_id UNIQUE;",
        // BM25 FTS on import_pages content (for Q&A without embeddings)
        "DEFINE INDEX IF NOT EXISTS ft_import_pages ON import_pages FIELDS title, content_md SEARCH ANALYZER note_analyzer BM25;",

        // ── Sitemap Titles（per-session 標題向量，用於 pending 頁查詢） ─
        "DEFINE TABLE IF NOT EXISTS sitemap_titles SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS entry_id   ON sitemap_titles TYPE string;",
        "DEFINE FIELD IF NOT EXISTS session_id ON sitemap_titles TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id   ON sitemap_titles TYPE string;",
        "DEFINE FIELD IF NOT EXISTS url        ON sitemap_titles TYPE string;",
        "DEFINE FIELD IF NOT EXISTS title      ON sitemap_titles TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS depth      ON sitemap_titles TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS embedding  ON sitemap_titles TYPE option<array<float>>;",
        "DEFINE INDEX IF NOT EXISTS idx_sitemap_session  ON sitemap_titles FIELDS vault_id, session_id;",
        "DEFINE INDEX IF NOT EXISTS idx_sitemap_entry_id ON sitemap_titles FIELDS vault_id, entry_id UNIQUE;",
        // FTS fallback on title
        "DEFINE INDEX IF NOT EXISTS ft_sitemap_titles ON sitemap_titles FIELDS title SEARCH ANALYZER note_analyzer BM25;",

        // ── Knowledge Items（使用者明確儲存的知識） ──────────────────
        "DEFINE TABLE IF NOT EXISTS knowledge_items SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS item_id       ON knowledge_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id      ON knowledge_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS session_id    ON knowledge_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS title         ON knowledge_items TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS source_refs   ON knowledge_items TYPE array DEFAULT [];",
        "DEFINE FIELD IF NOT EXISTS ai_summary    ON knowledge_items TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS created_at    ON knowledge_items TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_ki_vault_id  ON knowledge_items FIELDS vault_id, item_id UNIQUE;",
        "DEFINE INDEX IF NOT EXISTS idx_ki_session   ON knowledge_items FIELDS vault_id, session_id;",
        "DEFINE INDEX IF NOT EXISTS idx_ki_created   ON knowledge_items FIELDS vault_id, created_at;",

        // chunks 表新增 item_id 欄位（關聯 knowledge_items；vault note chunks 為 NULL）
        "DEFINE FIELD IF NOT EXISTS item_id ON chunks TYPE option<string>;",
        "DEFINE INDEX IF NOT EXISTS idx_chunks_item_id ON chunks FIELDS vault_id, item_id;",

        // ── agent_skills：使用者從知識項目啟用的行為規範，注入 agent system prompt ──
        "DEFINE TABLE IF NOT EXISTS agent_skills SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS skill_id             ON agent_skills TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id             ON agent_skills TYPE string;",
        "DEFINE FIELD IF NOT EXISTS knowledge_item_id    ON agent_skills TYPE string;",
        "DEFINE FIELD IF NOT EXISTS title                ON agent_skills TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS trigger              ON agent_skills TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS behavior             ON agent_skills TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS auto_tool_calls      ON agent_skills TYPE array<string> DEFAULT [];",
        "DEFINE FIELD IF NOT EXISTS is_active            ON agent_skills TYPE bool DEFAULT true;",
        "DEFINE FIELD IF NOT EXISTS trigger_count        ON agent_skills TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS last_triggered_at    ON agent_skills TYPE option<datetime>;",
        "DEFINE FIELD IF NOT EXISTS trigger_embedding    ON agent_skills TYPE option<array<float>>;",
        "DEFINE FIELD IF NOT EXISTS injection_mode       ON agent_skills TYPE string DEFAULT 'passive';",
        "DEFINE FIELD IF NOT EXISTS created_at           ON agent_skills TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_agent_skills_vault  ON agent_skills FIELDS vault_id, skill_id UNIQUE;",
        "DEFINE INDEX IF NOT EXISTS idx_agent_skills_ki     ON agent_skills FIELDS vault_id, knowledge_item_id;",
        "DEFINE INDEX IF NOT EXISTS idx_agent_skills_active ON agent_skills FIELDS vault_id, is_active;",

        // ── skill_usage_log：每次技能觸發記錄一筆，供趨勢圖使用 ──────────────
        "DEFINE TABLE IF NOT EXISTS skill_usage_log SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS log_id       ON skill_usage_log TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id     ON skill_usage_log TYPE string;",
        "DEFINE FIELD IF NOT EXISTS skill_id     ON skill_usage_log TYPE string;",
        "DEFINE FIELD IF NOT EXISTS triggered_at ON skill_usage_log TYPE datetime DEFAULT time::now();",
        "DEFINE INDEX IF NOT EXISTS idx_sul_vault   ON skill_usage_log FIELDS vault_id;",
        "DEFINE INDEX IF NOT EXISTS idx_sul_skill   ON skill_usage_log FIELDS vault_id, skill_id;",
        "DEFINE INDEX IF NOT EXISTS idx_sul_date    ON skill_usage_log FIELDS vault_id, triggered_at;",
    ];

    // Batch all DDL into one query to avoid N sequential round-trips.
    let batch = stmts.join("\n");
    db.query(batch)
        .await
        .map_err(|e| crate::error::AppError::Database(format!("schema error: {e}")))?;

    // 插入預設設定（只在 key 不存在時插入）
    insert_default_settings(db).await?;

    Ok(())
}

async fn insert_default_settings(db: &Surreal<Db>) -> crate::error::Result<()> {
    // 所有值均為程式內建常數，直接內嵌成 batch 避免 N 次 round-trip
    let defaults: &[(&str, &str)] = &[
        ("vault_path",          ""),
        ("theme",               "dark"),
        ("auto_save_mode",      "afterDelay"),
        ("auto_save_delay",     "1000"),
        ("whisper_cli_path",    ""),
        ("whisper_model_path",  ""),
        ("whisper_language",    "auto"),
        ("whisper_auto_insert", "true"),
        ("import_max_depth",    "3"),
        ("import_max_pages",    "50"),
        ("ai_provider",         ""),
        ("ai_model",            "gpt-4o"),
        ("ai_base_url",         "https://api.openai.com/v1"),
        ("ai_enable_topics",    "true"),
        ("ai_enable_summary",   "true"),
        ("ai_enable_vision",    "true"),
        ("llm_model_path",      ""),
        ("llama_cli_path",      ""),
        ("last_open_note",      ""),
        ("window_width",        "1400"),
        ("window_height",       "900"),
        ("sidebar_width",       "240"),
        ("graph_panel_width",   "320"),
        ("onboarding_done",     "false"),
        ("recent_vaults",       "[]"),
        ("sort_orders",         "{}"),
        ("font_sans",           ""),
        ("font_mono",           ""),
        ("editor_font_size",    "14"),
        ("ui_font_size",        "14"),
        ("graph_font_size",     "11"),
        ("debug_mode",          "false"),
        ("voice_process_mode",  "none"),
        ("enable_chat",         "false"),
        ("llama_server_port",   "8080"),
        ("whisper_server_port", "8081"),
        ("enable_auto_memory",  "false"),
        ("memory_threshold",    "20"),
    ];
    let batch = defaults.iter()
        .map(|(k, v)| format!(
            "INSERT INTO settings (key, value) VALUES ('{}', '{}') ON DUPLICATE KEY UPDATE key = key;",
            k, v
        ))
        .collect::<Vec<_>>()
        .join("\n");
    db.query(batch)
        .await
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    Ok(())
}

