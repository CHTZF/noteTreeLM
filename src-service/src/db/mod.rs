pub mod sqlite;
pub mod seeds;

use std::path::PathBuf;
use surrealdb::{engine::any::Any, Surreal};

pub type SurrealDb = Surreal<Any>;

/// 初始化 SurrealDB：embedded KV 模式（daemon 專用）
pub async fn init_db(data_dir: &PathBuf) -> Result<SurrealDb, surrealdb::Error> {
    let db_path = data_dir.join("service.db");
    std::fs::create_dir_all(&db_path).ok();
    let connection_str = format!("surrealkv://{}", db_path.display());

    let db = surrealdb::engine::any::connect(connection_str).await?;
    db.use_ns("notetreelm").use_db("service").await?;

    run_migrations(&db).await?;
    tracing::info!("SurrealDB initialized at {}", db_path.display());
    Ok(db)
}

pub async fn run_migrations_pub(db: &SurrealDb) -> Result<(), surrealdb::Error> {
    run_migrations(db).await
}

async fn run_migrations(db: &SurrealDb) -> Result<(), surrealdb::Error> {
    // Pre-flight: drop legacy BM25 / SEARCH indexes before the main DDL transaction.
    // These caused SurrealDB M-tree (TreeWrite) corruption on bulk note deletes.
    // They were redundant — SQLite FTS5 handles all full-text search.
    // Running as separate queries (not inside BEGIN TRANSACTION) ensures they succeed
    // even if the tree state is already corrupted.
    for stmt in &[
        "REMOVE INDEX IF EXISTS idx_notes_fts ON notes",
        "REMOVE INDEX IF EXISTS idx_import_pages_fts ON import_pages",
    ] {
        if let Err(e) = db.query(*stmt).await {
            tracing::warn!("Pre-flight index cleanup failed ({}): {}", stmt, e);
        }
    }

    let stmts: &[&str] = &[
        // ── daemon infra ────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS daemon_secrets SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS `key`   ON daemon_secrets TYPE string;",
        "DEFINE FIELD IF NOT EXISTS `value` ON daemon_secrets TYPE string;",
        "DEFINE INDEX IF NOT EXISTS idx_ds_key ON daemon_secrets FIELDS `key` UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS device_tokens SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS device_id          ON device_tokens TYPE string;",
        "DEFINE FIELD IF NOT EXISTS device_name        ON device_tokens TYPE string;",
        "DEFINE FIELD IF NOT EXISTS refresh_token_hash ON device_tokens TYPE string;",
        "DEFINE FIELD IF NOT EXISTS refresh_expires_at ON device_tokens TYPE int;",
        "DEFINE FIELD IF NOT EXISTS scope              ON device_tokens TYPE string;",
        "DEFINE FIELD IF NOT EXISTS created_at         ON device_tokens TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS last_used_at       ON device_tokens TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS revoked            ON device_tokens TYPE bool DEFAULT false;",
        "DEFINE INDEX IF NOT EXISTS idx_dt_id ON device_tokens FIELDS device_id UNIQUE;",

        // ── scheduler ───────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS scheduled_tasks SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS task_id              ON scheduled_tasks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id             ON scheduled_tasks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS account_id           ON scheduled_tasks TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS description          ON scheduled_tasks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS agent_def_name       ON scheduled_tasks TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS agent_prompt         ON scheduled_tasks TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS run_at_ts            ON scheduled_tasks TYPE int;",
        "DEFINE FIELD IF NOT EXISTS repeat_interval_secs ON scheduled_tasks TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS status               ON scheduled_tasks TYPE string DEFAULT 'pending';",
        "DEFINE FIELD IF NOT EXISTS created_at           ON scheduled_tasks TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_st_task_id       ON scheduled_tasks FIELDS task_id UNIQUE;",
        "DEFINE INDEX IF NOT EXISTS idx_st_vault_agent   ON scheduled_tasks FIELDS vault_id, account_id, agent_def_name;",

        // ── users & sessions ─────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS users SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS username     ON users TYPE string;",
        "DEFINE FIELD IF NOT EXISTS password_hash ON users TYPE string;",
        "DEFINE FIELD IF NOT EXISTS created_at   ON users TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_users_username ON users FIELDS username UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS sessions SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS token         ON sessions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS username      ON sessions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS expires_at    ON sessions TYPE int;",
        "DEFINE FIELD IF NOT EXISTS auth_provider ON sessions TYPE string DEFAULT 'local';",
        "DEFINE FIELD IF NOT EXISTS created_at    ON sessions TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_sessions_token ON sessions FIELDS token UNIQUE;",

        // ── settings ─────────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS `settings` SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS `key`      ON `settings` TYPE string;",
        "DEFINE FIELD IF NOT EXISTS `value`    ON `settings` TYPE string;",
        "DEFINE FIELD IF NOT EXISTS updated_at ON `settings` TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_settings_key ON `settings` FIELDS `key` UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS user_settings SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS username   ON user_settings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS `key`      ON user_settings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS `value`    ON user_settings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS updated_at ON user_settings TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_user_settings_uk ON user_settings FIELDS username, `key` UNIQUE;",

        // ── vault states & vaults ────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS vault_states SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_path     ON vault_states TYPE string;",
        "DEFINE FIELD IF NOT EXISTS last_open_note ON vault_states TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS updated_at     ON vault_states TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_vault_states_path ON vault_states FIELDS vault_path UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS vaults SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id   ON vaults TYPE string;",
        "DEFINE FIELD IF NOT EXISTS path       ON vaults TYPE string;",
        "DEFINE FIELD IF NOT EXISTS account_id ON vaults TYPE string;",
        "DEFINE FIELD OVERWRITE     account_id ON vaults TYPE string;",
        "DEFINE FIELD IF NOT EXISTS created_at ON vaults TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_vaults_vault_id         ON vaults FIELDS vault_id UNIQUE;",
        "DEFINE INDEX IF NOT EXISTS idx_vaults_path_account     ON vaults FIELDS path, account_id UNIQUE;",

        // ── conversations ────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS conversations SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS id                ON conversations TYPE string;",
        "DEFINE FIELD IF NOT EXISTS account_id        ON conversations TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id          ON conversations TYPE string;",
        "DEFINE FIELD IF NOT EXISTS mode              ON conversations TYPE string DEFAULT 'chat';",
        "DEFINE FIELD IF NOT EXISTS title             ON conversations TYPE string;",
        "DEFINE FIELD IF NOT EXISTS messages_json     ON conversations TYPE string DEFAULT '[]';",
        "DEFINE FIELD IF NOT EXISTS memory_processed_at        ON conversations TYPE option<int>;",
        "DEFINE FIELD IF NOT EXISTS memory_processed_msg_count ON conversations TYPE option<int>;",
        "DEFINE FIELD IF NOT EXISTS created_at        ON conversations TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS updated_at        ON conversations TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_conversations_id ON conversations FIELDS id UNIQUE;",

        // ── pending plans ────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS pending_plans SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS conversation_id     ON pending_plans TYPE string;",
        "DEFINE FIELD IF NOT EXISTS deferred_tools_json ON pending_plans TYPE string DEFAULT '[]';",
        "DEFINE FIELD IF NOT EXISTS confirm_centroid    ON pending_plans TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS cancel_centroid     ON pending_plans TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS interrupt_centroid  ON pending_plans TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS created_at          ON pending_plans TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_pp_conv_id ON pending_plans FIELDS conversation_id UNIQUE;",

        // ── notes ────────────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS notes SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id    ON notes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS path        ON notes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS title       ON notes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS content     ON notes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS frontmatter ON notes TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS word_count  ON notes TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS created_at  ON notes TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS modified_at ON notes TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS checksum    ON notes TYPE option<string>;",
        "DEFINE INDEX IF NOT EXISTS idx_notes_vault_path ON notes FIELDS vault_id, path UNIQUE;",

        // ── links ────────────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS links SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id     ON links TYPE string;",
        "DEFINE FIELD IF NOT EXISTS source_path  ON links TYPE string;",
        "DEFINE FIELD IF NOT EXISTS target_title ON links TYPE string;",
        "DEFINE FIELD IF NOT EXISTS target_path  ON links TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS link_type    ON links TYPE string DEFAULT 'wiki';",
        "DEFINE FIELD IF NOT EXISTS raw_text     ON links TYPE string;",
        "DEFINE FIELD IF NOT EXISTS alias        ON links TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS heading      ON links TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS line_number  ON links TYPE int DEFAULT 0;",

        // ── tags ─────────────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS tags SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id  ON tags TYPE string;",
        "DEFINE FIELD IF NOT EXISTS note_path ON tags TYPE string;",
        "DEFINE FIELD IF NOT EXISTS tag       ON tags TYPE string;",
        "DEFINE INDEX IF NOT EXISTS idx_tags_vault_path_tag ON tags FIELDS vault_id, note_path, tag UNIQUE;",

        // ── chunks ───────────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS chunks SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id   ON chunks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS chunk_id   ON chunks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS file_path  ON chunks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS section    ON chunks TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS content    ON chunks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS links      ON chunks TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS chunk_type ON chunks TYPE string DEFAULT 'text';",
        "DEFINE FIELD IF NOT EXISTS word_count ON chunks TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS updated_at ON chunks TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS status     ON chunks TYPE string DEFAULT 'active';",
        "DEFINE FIELD IF NOT EXISTS item_id    ON chunks TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS embedding  ON chunks TYPE option<array>;",
        "DEFINE INDEX IF NOT EXISTS idx_chunks_id ON chunks FIELDS chunk_id UNIQUE;",

        // ── import sessions & pages ──────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS import_sessions SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id        ON import_sessions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS session_id      ON import_sessions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS conversation_id ON import_sessions TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS seed_url        ON import_sessions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS site_name       ON import_sessions TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS root_folder     ON import_sessions TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS site_outline    ON import_sessions TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS crawl_policy    ON import_sessions TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS status          ON import_sessions TYPE string DEFAULT 'pending';",
        "DEFINE FIELD IF NOT EXISTS auto_update     ON import_sessions TYPE bool DEFAULT false;",
        "DEFINE FIELD IF NOT EXISTS created_at      ON import_sessions TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS updated_at      ON import_sessions TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_import_sessions_id ON import_sessions FIELDS session_id UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS import_pages SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id      ON import_pages TYPE string;",
        "DEFINE FIELD IF NOT EXISTS page_id       ON import_pages TYPE string;",
        "DEFINE FIELD IF NOT EXISTS session_id    ON import_pages TYPE string;",
        "DEFINE FIELD IF NOT EXISTS url           ON import_pages TYPE string;",
        "DEFINE FIELD IF NOT EXISTS title         ON import_pages TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS parent_url    ON import_pages TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS depth         ON import_pages TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS note_path     ON import_pages TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS content_md    ON import_pages TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS content_hash  ON import_pages TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS http_etag     ON import_pages TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS status        ON import_pages TYPE string DEFAULT 'pending';",
        "DEFINE FIELD IF NOT EXISTS last_crawled  ON import_pages TYPE option<int>;",
        "DEFINE FIELD IF NOT EXISTS created_at    ON import_pages TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS embedding     ON import_pages TYPE option<array>;",
        "DEFINE INDEX IF NOT EXISTS idx_import_pages_id ON import_pages FIELDS page_id UNIQUE;",

        // ── knowledge items ──────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS knowledge_items SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS item_id     ON knowledge_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id    ON knowledge_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS session_id  ON knowledge_items TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS title       ON knowledge_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS source_refs ON knowledge_items TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS ai_summary  ON knowledge_items TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS created_at  ON knowledge_items TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_knowledge_items_id ON knowledge_items FIELDS item_id UNIQUE;",

        // ── agent skills ─────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS agent_skills SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS skill_id             ON agent_skills TYPE string;",
        "DEFINE FIELD IF NOT EXISTS account_id           ON agent_skills TYPE string;",
        "DEFINE FIELD OVERWRITE     account_id           ON agent_skills TYPE string;",
        "DEFINE FIELD IF NOT EXISTS knowledge_item_id    ON agent_skills TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS title                ON agent_skills TYPE string;",
        "DEFINE FIELD IF NOT EXISTS trigger              ON agent_skills TYPE string;",
        "DEFINE FIELD IF NOT EXISTS behavior             ON agent_skills TYPE string;",
        "DEFINE FIELD IF NOT EXISTS tool_calls           ON agent_skills TYPE option<array>;",
        "DEFINE FIELD OVERWRITE     tool_calls           ON agent_skills TYPE option<array>;",
        "DEFINE FIELD IF NOT EXISTS is_active            ON agent_skills TYPE bool DEFAULT true;",
        "DEFINE FIELD IF NOT EXISTS trigger_count        ON agent_skills TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS last_triggered_at    ON agent_skills TYPE option<int>;",
        "DEFINE FIELD IF NOT EXISTS injection_mode       ON agent_skills TYPE string DEFAULT 'system';",
        "DEFINE FIELD IF NOT EXISTS agent_scope          ON agent_skills TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS need_tool_chain      ON agent_skills TYPE bool DEFAULT false;",
        "DEFINE FIELD IF NOT EXISTS tool_chain_order     ON agent_skills TYPE array;",
        "DEFINE FIELD IF NOT EXISTS created_at           ON agent_skills TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS embedding            ON agent_skills TYPE option<array>;",
        "DEFINE INDEX IF NOT EXISTS idx_agent_skills_id ON agent_skills FIELDS skill_id UNIQUE;",

        // ── agent tools ──────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS agent_tools SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS tool_id     ON agent_tools TYPE string;",
        "DEFINE FIELD IF NOT EXISTS name        ON agent_tools TYPE string;",
        "DEFINE FIELD IF NOT EXISTS description ON agent_tools TYPE string;",
        "DEFINE FIELD IF NOT EXISTS schema_json ON agent_tools TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS is_active   ON agent_tools TYPE bool DEFAULT true;",
        "DEFINE FIELD IF NOT EXISTS is_builtin  ON agent_tools TYPE bool DEFAULT false;",
        "DEFINE FIELD IF NOT EXISTS created_at  ON agent_tools TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_agent_tools_id ON agent_tools FIELDS tool_id UNIQUE;",

        // ── agent definitions ────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS agent_definitions SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS def_id        ON agent_definitions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS account_id    ON agent_definitions TYPE string;",
        "DEFINE FIELD OVERWRITE     account_id    ON agent_definitions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS name          ON agent_definitions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS description   ON agent_definitions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS kind          ON agent_definitions TYPE string DEFAULT 'chat';",
        "DEFINE FIELD IF NOT EXISTS skill_ids     ON agent_definitions TYPE option<array>;",
        "DEFINE FIELD OVERWRITE     skill_ids     ON agent_definitions TYPE option<array>;",
        "DEFINE FIELD IF NOT EXISTS tool_names    ON agent_definitions TYPE option<array>;",
        "DEFINE FIELD OVERWRITE     tool_names    ON agent_definitions TYPE option<array>;",
        "DEFINE FIELD IF NOT EXISTS system_prompt ON agent_definitions TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS max_rounds    ON agent_definitions TYPE int DEFAULT 10;",
        "DEFINE FIELD IF NOT EXISTS is_active     ON agent_definitions TYPE bool DEFAULT true;",
        "DEFINE FIELD IF NOT EXISTS is_builtin    ON agent_definitions TYPE bool DEFAULT false;",
        "DEFINE FIELD IF NOT EXISTS trigger       ON agent_definitions TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS created_at    ON agent_definitions TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_agent_defs_id ON agent_definitions FIELDS def_id UNIQUE;",

        // ── skill usage log ──────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS skill_usage_log SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS log_id       ON skill_usage_log TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id     ON skill_usage_log TYPE string;",
        "DEFINE FIELD IF NOT EXISTS skill_id     ON skill_usage_log TYPE string;",
        "DEFINE FIELD IF NOT EXISTS triggered_at ON skill_usage_log TYPE int DEFAULT 0;",

        // ── activity patterns ────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS activity_patterns SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id         ON activity_patterns TYPE string;",
        "DEFINE FIELD IF NOT EXISTS signature        ON activity_patterns TYPE string;",
        "DEFINE FIELD IF NOT EXISTS score            ON activity_patterns TYPE float DEFAULT 0.0;",
        "DEFINE FIELD IF NOT EXISTS trigger_count    ON activity_patterns TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS speak_count      ON activity_patterns TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS deprecated       ON activity_patterns TYPE bool DEFAULT false;",
        "DEFINE FIELD IF NOT EXISTS semantic_intent  ON activity_patterns TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS last_triggered_at ON activity_patterns TYPE option<int>;",
        "DEFINE FIELD IF NOT EXISTS created_at       ON activity_patterns TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS updated_at       ON activity_patterns TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_activity_patterns_vs ON activity_patterns FIELDS vault_id, signature UNIQUE;",

        // ── assets ───────────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS assets SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id   ON assets TYPE string;",
        "DEFINE FIELD IF NOT EXISTS file_path  ON assets TYPE string;",
        "DEFINE FIELD IF NOT EXISTS mime_type  ON assets TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS file_size  ON assets TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS created_at ON assets TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_assets_vault_path ON assets FIELDS vault_id, file_path UNIQUE;",

        // ── trash items ──────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS trash_items SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id       ON trash_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS item_id        ON trash_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS original_path  ON trash_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS name           ON trash_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS title          ON trash_items TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS trash_filename ON trash_items TYPE string;",
        "DEFINE FIELD IF NOT EXISTS deleted_at     ON trash_items TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_trash_items_id ON trash_items FIELDS item_id UNIQUE;",

        // ── imports ──────────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS imports SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id   ON imports TYPE string;",
        "DEFINE FIELD IF NOT EXISTS source_url ON imports TYPE string;",
        "DEFINE FIELD IF NOT EXISTS note_path  ON imports TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS status     ON imports TYPE string DEFAULT 'pending';",
        "DEFINE FIELD IF NOT EXISTS created_at ON imports TYPE int DEFAULT 0;",

        // ── kb suggestions ───────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS kb_suggestions SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS suggestion_id ON kb_suggestions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id      ON kb_suggestions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS session_id    ON kb_suggestions TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS page_id       ON kb_suggestions TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS title         ON kb_suggestions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS template      ON kb_suggestions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS content       ON kb_suggestions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS reason        ON kb_suggestions TYPE string;",
        "DEFINE FIELD IF NOT EXISTS created_at    ON kb_suggestions TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_kb_suggestions_id ON kb_suggestions FIELDS suggestion_id UNIQUE;",

        // ── memory facts ─────────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS memory_facts SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS fact_id     ON memory_facts TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id    ON memory_facts TYPE string;",
        "DEFINE FIELD IF NOT EXISTS account_id  ON memory_facts TYPE string DEFAULT '';",
        "DEFINE FIELD IF NOT EXISTS conv_id     ON memory_facts TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS content     ON memory_facts TYPE string;",
        "DEFINE FIELD IF NOT EXISTS category    ON memory_facts TYPE string DEFAULT 'general';",
        "DEFINE FIELD IF NOT EXISTS expires_at  ON memory_facts TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS created_at  ON memory_facts TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS embedding   ON memory_facts TYPE option<array>;",
        "DEFINE FIELD IF NOT EXISTS inject_count ON memory_facts TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_memory_facts_id     ON memory_facts FIELDS fact_id UNIQUE;",
        "DEFINE INDEX IF NOT EXISTS idx_memory_facts_fts    ON memory_facts FIELDS content SEARCH ANALYZER noteanalyzer BM25;",
        "DEFINE INDEX IF NOT EXISTS idx_memory_facts_lookup ON memory_facts FIELDS vault_id, account_id, expires_at;",

        // ── response ratings ─────────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS response_ratings SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS rating_id        ON response_ratings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id         ON response_ratings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS conversation_id  ON response_ratings TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS content_hash     ON response_ratings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS rating           ON response_ratings TYPE string;",
        "DEFINE FIELD IF NOT EXISTS created_at       ON response_ratings TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_response_ratings_id ON response_ratings FIELDS rating_id UNIQUE;",

        // ── graph nodes & edges ──────────────────────────────────────────────
        "DEFINE TABLE IF NOT EXISTS graph_nodes SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id   ON graph_nodes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS node_id    ON graph_nodes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS node_type  ON graph_nodes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS label      ON graph_nodes TYPE string;",
        "DEFINE FIELD IF NOT EXISTS url        ON graph_nodes TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS file_path  ON graph_nodes TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS metadata   ON graph_nodes TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS created_at ON graph_nodes TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_graph_nodes_id ON graph_nodes FIELDS node_id UNIQUE;",

        "DEFINE TABLE IF NOT EXISTS graph_edges SCHEMALESS;",
        "DEFINE FIELD IF NOT EXISTS vault_id  ON graph_edges TYPE string;",
        "DEFINE FIELD IF NOT EXISTS source_id ON graph_edges TYPE string;",
        "DEFINE FIELD IF NOT EXISTS target_id ON graph_edges TYPE string;",
        "DEFINE FIELD IF NOT EXISTS edge_type ON graph_edges TYPE string DEFAULT 'link';",
        "DEFINE FIELD IF NOT EXISTS weight    ON graph_edges TYPE float DEFAULT 1.0;",
    ];

    // Wrap all DDL in a single transaction to avoid concurrent-migration conflicts
    let combined = format!("BEGIN TRANSACTION;\n{}\nCOMMIT TRANSACTION;", stmts.join("\n"));
    db.query(&combined).await?;
    Ok(())
}
