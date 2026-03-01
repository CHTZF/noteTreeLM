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

pub async fn get_vault_last_note(pool: &SqlitePool, vault_path: &str) -> crate::error::Result<Option<String>> {
    let note = sqlx::query_scalar::<_, String>(
        "SELECT last_open_note FROM vault_states WHERE vault_path = ?"
    )
    .bind(vault_path)
    .fetch_optional(pool)
    .await?;
    Ok(note.filter(|s| !s.is_empty()))
}

pub async fn set_vault_last_note(pool: &SqlitePool, vault_path: &str, note_path: &str) -> crate::error::Result<()> {
    sqlx::query(
        "INSERT INTO vault_states(vault_path, last_open_note, updated_at)
         VALUES (?, ?, strftime('%s','now'))
         ON CONFLICT(vault_path) DO UPDATE SET last_open_note = excluded.last_open_note, updated_at = excluded.updated_at"
    )
    .bind(vault_path)
    .bind(note_path)
    .execute(pool)
    .await?;
    Ok(())
}
