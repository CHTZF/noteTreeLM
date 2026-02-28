use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use std::path::Path;

pub mod queries;

pub async fn init_db(app_data_dir: &Path) -> crate::error::Result<SqlitePool> {
    let db_path = app_data_dir.join("notetreelm.db");

    // 確保目錄存在
    tokio::fs::create_dir_all(app_data_dir).await?;

    let db_url = format!("sqlite://{}?mode=rwc", db_path.display());

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;

    run_migrations(&pool).await?;

    Ok(pool)
}

async fn run_migrations(pool: &SqlitePool) -> crate::error::Result<()> {
    let migration_sql = include_str!("../../migrations/001_initial.sql");

    // 取得單一連線，用 raw_sql 一次執行完整 SQL（含 triggers 和多行語句）
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;

    sqlx::raw_sql(migration_sql)
        .execute(&mut *conn)
        .await
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;

    Ok(())
}
