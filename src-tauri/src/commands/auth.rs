use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Manager;
use crate::state::AppState;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SessionInfo {
    pub token: String,
    pub username: String,
    pub expires_at: i64,
}

fn hash_password(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn session_path(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path().app_data_dir().expect("app_data_dir").join("session.json")
}

#[tauri::command]
pub async fn login(
    username: String,
    password: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<SessionInfo, String> {
    let hash = hash_password(&password);

    let row = sqlx::query("SELECT username FROM users WHERE username = ? AND password_hash = ?")
        .bind(&username)
        .bind(&hash)
        .fetch_optional(&state.settings_db)
        .await
        .map_err(|e| e.to_string())?;

    if row.is_none() {
        return Err("帳號或密碼錯誤".to_string());
    }

    let token = uuid::Uuid::new_v4().to_string();
    let now = chrono::Local::now().timestamp();
    let expires_at = now + 30 * 24 * 3600; // 30 days

    let session = SessionInfo { token, username, expires_at };

    let path = session_path(&app);
    let json = serde_json::to_string(&session).map_err(|e| e.to_string())?;
    tokio::fs::write(&path, json).await.map_err(|e| e.to_string())?;

    Ok(session)
}

#[tauri::command]
pub async fn logout(app: tauri::AppHandle) -> Result<(), String> {
    let path = session_path(&app);
    if path.exists() {
        tokio::fs::remove_file(&path).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn get_session(app: tauri::AppHandle) -> Result<Option<SessionInfo>, String> {
    let path = session_path(&app);
    if !path.exists() {
        return Ok(None);
    }

    let json = tokio::fs::read_to_string(&path).await.map_err(|e| e.to_string())?;
    let session: SessionInfo = serde_json::from_str(&json).map_err(|e| e.to_string())?;

    let now = chrono::Local::now().timestamp();
    if session.expires_at <= now {
        let _ = tokio::fs::remove_file(&path).await;
        return Ok(None);
    }

    Ok(Some(session))
}

#[tauri::command]
pub async fn change_password(
    current_password: String,
    new_password: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let path = session_path(&app);
    if !path.exists() {
        return Err("未登入".to_string());
    }
    let json = tokio::fs::read_to_string(&path).await.map_err(|e| e.to_string())?;
    let session: SessionInfo = serde_json::from_str(&json).map_err(|e| e.to_string())?;

    let current_hash = hash_password(&current_password);
    let row = sqlx::query("SELECT id FROM users WHERE username = ? AND password_hash = ?")
        .bind(&session.username)
        .bind(&current_hash)
        .fetch_optional(&state.settings_db)
        .await
        .map_err(|e| e.to_string())?;

    if row.is_none() {
        return Err("目前密碼錯誤".to_string());
    }

    let new_hash = hash_password(&new_password);
    sqlx::query("UPDATE users SET password_hash = ? WHERE username = ?")
        .bind(&new_hash)
        .bind(&session.username)
        .execute(&state.settings_db)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}
