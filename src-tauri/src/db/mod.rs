use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use std::path::Path;

pub mod queries;

/// 初始化帳號層級設定 DB（app_data_dir/settings.db）
pub async fn init_settings_db(app_data_dir: &Path) -> crate::error::Result<SqlitePool> {
    let db_path = app_data_dir.join("settings.db");
    tokio::fs::create_dir_all(app_data_dir).await?;
    let db_url = format!("sqlite://{}?mode=rwc", db_path.to_string_lossy());
    let pool = connect(db_url).await?;
    run_migrations(&pool, include_str!("../../migrations/settings_001.sql")).await?;
    Ok(pool)
}

/// 初始化 Vault DB（vault_path/.notetreelm.db）
pub async fn init_vault_db(vault_path: &Path) -> crate::error::Result<SqlitePool> {
    let db_path = vault_path.join(".notetreelm.db");
    let db_url = format!("sqlite://{}?mode=rwc", db_path.to_string_lossy());
    let pool = connect(db_url).await?;
    run_migrations(&pool, include_str!("../../migrations/vault_001.sql")).await?;
    Ok(pool)
}

async fn connect(db_url: String) -> crate::error::Result<SqlitePool> {
    SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .map_err(|e| crate::error::AppError::Database(e.to_string()))
}

async fn run_migrations(pool: &SqlitePool, sql: &str) -> crate::error::Result<()> {
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    Ok(())
}
