use std::path::PathBuf;
use surrealdb::{engine::any::Any, Surreal};

pub type SurrealDb = Surreal<Any>;

/// 初始化 SurrealDB：embedded KV 模式（daemon 專用）
pub async fn init_db(data_dir: &PathBuf) -> Result<SurrealDb, surrealdb::Error> {
    let db_path = data_dir.join("service.db");
    std::fs::create_dir_all(&db_path).ok();
    let connection_str = format!("surrealkv://{}", db_path.display());

    let db = surrealdb::engine::any::connect(connection_str).await?;
    db.use_ns("notetreetlm").use_db("service").await?;

    run_migrations(&db).await?;
    tracing::info!("SurrealDB initialized at {}", db_path.display());
    Ok(db)
}

pub async fn run_migrations_pub(db: &SurrealDb) -> Result<(), surrealdb::Error> {
    run_migrations(db).await
}

async fn run_migrations(db: &SurrealDb) -> Result<(), surrealdb::Error> {
    let stmts = [
        "DEFINE TABLE IF NOT EXISTS daemon_secrets SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS key   ON daemon_secrets TYPE string;",
        "DEFINE FIELD IF NOT EXISTS value ON daemon_secrets TYPE string;",
        "DEFINE INDEX IF NOT EXISTS idx_ds_key ON daemon_secrets FIELDS key UNIQUE;",
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
        // Scheduled tasks table (daemon-owned scheduler)
        "DEFINE TABLE IF NOT EXISTS scheduled_tasks SCHEMAFULL;",
        "DEFINE FIELD IF NOT EXISTS task_id              ON scheduled_tasks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS vault_id             ON scheduled_tasks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS description          ON scheduled_tasks TYPE string;",
        "DEFINE FIELD IF NOT EXISTS agent_type           ON scheduled_tasks TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS agent_prompt         ON scheduled_tasks TYPE option<string>;",
        "DEFINE FIELD IF NOT EXISTS run_at_ts            ON scheduled_tasks TYPE int;",
        "DEFINE FIELD IF NOT EXISTS repeat_interval_secs ON scheduled_tasks TYPE int DEFAULT 0;",
        "DEFINE FIELD IF NOT EXISTS status               ON scheduled_tasks TYPE string DEFAULT 'pending';",
        "DEFINE FIELD IF NOT EXISTS created_at           ON scheduled_tasks TYPE int DEFAULT 0;",
        "DEFINE INDEX IF NOT EXISTS idx_st_task_id ON scheduled_tasks FIELDS task_id UNIQUE;",
    ];
    for stmt in &stmts {
        db.query(*stmt).await?;
    }
    Ok(())
}
