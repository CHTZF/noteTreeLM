use serde::Deserialize;
use crate::db::surreal::SurrealDb;

// ── 泛型輔助：從 SELECT 結果取第一個 value 欄位 ─────────────────────────
#[derive(Deserialize)]
struct ValueRow {
    value: String,
}

// ── Global settings ──────────────────────────────────────────────────────

pub async fn get_setting(db: &SurrealDb, key: &str) -> crate::error::Result<Option<String>> {
    let mut resp = db
        .query("SELECT value FROM settings WHERE key = $key LIMIT 1")
        .bind(("key", key.to_owned()))
        .await
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    let rows: Vec<ValueRow> = resp
        .take(0)
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    Ok(rows.into_iter().next().map(|r| r.value))
}

pub async fn set_setting(db: &SurrealDb, key: &str, value: &str) -> crate::error::Result<()> {
    db.query(
        "INSERT INTO settings (key, value) VALUES ($key, $value)
         ON DUPLICATE KEY UPDATE value = $value, updated_at = time::now()",
    )
    .bind(("key", key.to_owned()))
    .bind(("value", value.to_owned()))
    .await
    .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    Ok(())
}

// ── Per-user settings ────────────────────────────────────────────────────

pub async fn get_user_setting(
    db: &SurrealDb,
    username: &str,
    key: &str,
) -> crate::error::Result<Option<String>> {
    let mut resp = db
        .query(
            "SELECT value FROM user_settings WHERE username = $username AND key = $key LIMIT 1",
        )
        .bind(("username", username.to_owned()))
        .bind(("key", key.to_owned()))
        .await
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    let rows: Vec<ValueRow> = resp
        .take(0)
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    Ok(rows.into_iter().next().map(|r| r.value))
}

pub async fn set_user_setting(
    db: &SurrealDb,
    username: &str,
    key: &str,
    value: &str,
) -> crate::error::Result<()> {
    db.query(
        "INSERT INTO user_settings (username, key, value)
         VALUES ($username, $key, $value)
         ON DUPLICATE KEY UPDATE value = $value, updated_at = time::now()",
    )
    .bind(("username", username.to_owned()))
    .bind(("key", key.to_owned()))
    .bind(("value", value.to_owned()))
    .await
    .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    Ok(())
}

// ── Vault last-open note ─────────────────────────────────────────────────

pub async fn get_vault_last_note(
    db: &SurrealDb,
    vault_path: &str,
) -> crate::error::Result<Option<String>> {
    #[derive(Deserialize)]
    struct Row {
        last_open_note: String,
    }
    let mut resp = db
        .query(
            "SELECT last_open_note FROM vault_states WHERE vault_path = $vault_path LIMIT 1",
        )
        .bind(("vault_path", vault_path.to_owned()))
        .await
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    let rows: Vec<Row> = resp
        .take(0)
        .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    Ok(rows
        .into_iter()
        .next()
        .map(|r| r.last_open_note)
        .filter(|s| !s.is_empty()))
}

pub async fn set_vault_last_note(
    db: &SurrealDb,
    vault_path: &str,
    note_path: &str,
) -> crate::error::Result<()> {
    db.query(
        "INSERT INTO vault_states (vault_path, last_open_note)
         VALUES ($vault_path, $note_path)
         ON DUPLICATE KEY UPDATE last_open_note = $note_path, updated_at = time::now()",
    )
    .bind(("vault_path", vault_path.to_owned()))
    .bind(("note_path", note_path.to_owned()))
    .await
    .map_err(|e| crate::error::AppError::Database(e.to_string()))?;
    Ok(())
}
