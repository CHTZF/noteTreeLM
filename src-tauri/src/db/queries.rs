use sqlx::SqlitePool;

pub async fn get_setting(pool: &SqlitePool, key: &str) -> crate::error::Result<Option<String>> {
    let row = sqlx::query_scalar::<_, String>(
        "SELECT value FROM settings WHERE key = ?"
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> crate::error::Result<()> {
    sqlx::query(
        "INSERT INTO settings(key, value, updated_at) VALUES (?, ?, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at"
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}
