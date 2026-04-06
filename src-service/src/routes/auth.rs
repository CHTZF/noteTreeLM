use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use bcrypt::{hash, verify, DEFAULT_COST};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::app_state::ApiState;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)]
pub struct LoginResponse {
    pub token: String,
    pub username: String,
    pub expires_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
}

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/session", get(session))
        .route("/auth/register", post(register))
        .route("/auth/google-upsert", post(google_upsert))
        .route("/auth/change-password", post(change_password))
        .route("/auth/generate-pairing-code", post(generate_pairing_code))
}

async fn generate_pairing_code(
    State(state): State<ApiState>,
) -> Json<Value> {
    let code = state.auth.generate_pairing_code().await;
    Json(json!({ "pairing_code": code, "expires_in": 300 }))
}

async fn login(
    State(state): State<ApiState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    #[derive(serde::Deserialize)]
    struct UserRow {
        username: String,
        password_hash: String,
    }

    let mut resp = state
        .db
        .query("SELECT username, password_hash FROM users WHERE username = $u")
        .bind(("u", req.username.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<UserRow> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let user = rows
        .into_iter()
        .next()
        .ok_or((StatusCode::UNAUTHORIZED, "Invalid credentials".to_string()))?;

    let pw = req.password.clone();
    let hash_str = user.password_hash.clone();
    let valid = tokio::task::spawn_blocking(move || verify(&pw, &hash_str))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("spawn: {e}")))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !valid {
        return Err((StatusCode::UNAUTHORIZED, "Invalid credentials".to_string()));
    }

    let token = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    let expires_at = now + 86400 * 30; // 30 days

    state
        .db
        .query("INSERT INTO sessions (token, username, expires_at, auth_provider, created_at) VALUES ($tok, $uname, $expires_at, 'local', $now)")
        .bind(("tok", token.clone()))
        .bind(("uname", user.username.clone()))
        .bind(("expires_at", expires_at))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Seed builtin skills/agents for this account on first login (idempotent)
    {
        let username_for_seed = user.username.clone();
        let db_for_seed = state.db.clone();
        let embed_url = state.daemon.embedding_url.read().await.clone();
        tokio::spawn(async move {
            crate::db::seeds::seed_builtins(&db_for_seed, &username_for_seed).await;
            crate::db::seeds::embed_skills_for_account(&db_for_seed, &username_for_seed, &embed_url).await;
        });
    }

    Ok(Json(json!({
        "token": token,
        "username": user.username,
        "expires_at": expires_at,
    })))
}

async fn logout(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let token = extract_bearer(&headers)
        .ok_or((StatusCode::UNAUTHORIZED, "Missing token".to_string()))?;

    state
        .db
        .query("DELETE FROM sessions WHERE token = $t")
        .bind(("t", token))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn session(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let token = match extract_bearer(&headers) {
        Some(t) => t,
        None => return Ok(Json(json!(null))),
    };

    #[derive(serde::Deserialize)]
    struct SessionRow {
        token: String,
        username: String,
        expires_at: i64,
        auth_provider: Option<String>,
    }

    let now = Utc::now().timestamp();
    let mut resp = state
        .db
        .query("SELECT token, username, expires_at, auth_provider FROM sessions WHERE token = $t AND expires_at > $now")
        .bind(("t", token))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<SessionRow> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match rows.into_iter().next() {
        Some(s) => Ok(Json(json!({
            "token": s.token,
            "username": s.username,
            "expires_at": s.expires_at,
            "auth_provider": s.auth_provider.unwrap_or_else(|| "local".to_string()),
        }))),
        None => Ok(Json(json!(null))),
    }
}

async fn register(
    State(state): State<ApiState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Check no existing user
    #[derive(serde::Deserialize)]
    struct CountRow {
        count: i64,
    }

    let mut resp = state
        .db
        .query("SELECT count() AS count FROM users WHERE username = $u GROUP ALL")
        .bind(("u", req.username.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<CountRow> = resp.take(0).unwrap_or_default();
    let count = rows.into_iter().next().map(|r| r.count).unwrap_or(0);
    if count > 0 {
        return Err((StatusCode::CONFLICT, "Username already exists".to_string()));
    }

    let pw = req.password.clone();
    let password_hash = tokio::task::spawn_blocking(move || hash(&pw, DEFAULT_COST))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("spawn: {e}")))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO users (username, password_hash, created_at) VALUES ($u, $ph, $now)")
        .bind(("u", req.username.clone()))
        .bind(("ph", password_hash))
        .bind(("now", now))
        .await
        .map_err(|e| {
            tracing::error!("register INSERT failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    // Seed on first registration
    {
        let username_for_seed = req.username.clone();
        let db_for_seed = state.db.clone();
        let embed_url = state.daemon.embedding_url.read().await.clone();
        tokio::spawn(async move {
            crate::db::seeds::seed_builtins(&db_for_seed, &username_for_seed).await;
            crate::db::seeds::embed_skills_for_account(&db_for_seed, &username_for_seed, &embed_url).await;
        });
    }

    Ok(Json(json!({ "ok": true, "username": req.username })))
}

/// Google OAuth upsert: create or update user by email, using sub as stable credential.
/// No bcrypt verification — caller (Tauri desktop) already verified with Google.
async fn google_upsert(
    State(state): State<ApiState>,
    Json(req): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let username = req["username"].as_str()
        .ok_or((StatusCode::BAD_REQUEST, "Missing username".to_string()))?
        .to_string();
    let sub = req["sub"].as_str()
        .ok_or((StatusCode::BAD_REQUEST, "Missing sub".to_string()))?;

    // bcrypt is CPU-bound; run in blocking thread to avoid blocking the tokio executor
    let sub_owned = sub.to_string();
    let password_hash = tokio::task::spawn_blocking(move || hash(&sub_owned, DEFAULT_COST))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("spawn_blocking: {e}")))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("bcrypt: {e}")))?;

    let now = Utc::now().timestamp();

    // Check if user already exists
    #[derive(serde::Deserialize)]
    struct UserRow { #[allow(dead_code)] username: String }
    let mut check = state
        .db
        .query("SELECT username FROM users WHERE username = $u LIMIT 1")
        .bind(("u", username.clone()))
        .await
        .map_err(|e| {
            tracing::error!("google_upsert SELECT failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;
    let existing: Vec<UserRow> = check.take(0).unwrap_or_default();

    if existing.is_empty() {
        // First Google login — insert user
        state
            .db
            .query("INSERT INTO users (username, password_hash, created_at) VALUES ($u, $ph, $now)")
            .bind(("u", username.clone()))
            .bind(("ph", password_hash.clone()))
            .bind(("now", now))
            .await
            .map_err(|e| {
                tracing::error!("google_upsert INSERT user failed: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?;
    } else {
        // Subsequent login — update password_hash (sub is stable, but keeps hash current)
        state
            .db
            .query("UPDATE users SET password_hash = $ph WHERE username = $u")
            .bind(("ph", password_hash.clone()))
            .bind(("u", username.clone()))
            .await
            .map_err(|e| {
                tracing::error!("google_upsert UPDATE user failed: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?;
    }

    // Create session
    let token = Uuid::new_v4().to_string();
    let expires_at = now + 86400 * 30;

    state
        .db
        .query("INSERT INTO sessions (token, username, expires_at, auth_provider, created_at) VALUES ($tok, $uname, $expires_at, 'google', $now)")
        .bind(("tok", token.clone()))
        .bind(("uname", username.clone()))
        .bind(("expires_at", expires_at))
        .bind(("now", now))
        .await
        .map_err(|e| {
            tracing::error!("google_upsert INSERT session failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    // Seed builtin skills/agents for this account (idempotent)
    {
        let username_for_seed = username.clone();
        let db_for_seed = state.db.clone();
        let embed_url = state.daemon.embedding_url.read().await.clone();
        tokio::spawn(async move {
            crate::db::seeds::seed_builtins(&db_for_seed, &username_for_seed).await;
            crate::db::seeds::embed_skills_for_account(&db_for_seed, &username_for_seed, &embed_url).await;
        });
    }

    tracing::info!("google_upsert: session created for {}", username);
    Ok(Json(json!({
        "token": token,
        "username": username,
        "expires_at": expires_at,
    })))
}

#[derive(serde::Deserialize)]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

async fn change_password(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Resolve username from Bearer token
    let token = extract_bearer(&headers)
        .ok_or((StatusCode::UNAUTHORIZED, "Missing token".to_string()))?;
    let now = Utc::now().timestamp();

    #[derive(serde::Deserialize)]
    struct SessionRow { username: String }
    let mut sr = state.db
        .query("SELECT username FROM sessions WHERE token = $t AND expires_at > $now")
        .bind(("t", token))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let username = sr.take::<Vec<SessionRow>>(0).unwrap_or_default()
        .into_iter().next()
        .map(|r| r.username)
        .ok_or((StatusCode::UNAUTHORIZED, "Invalid or expired session".to_string()))?;

    // Verify current password
    #[derive(serde::Deserialize)]
    struct UserRow { password_hash: String }
    let mut resp = state.db
        .query("SELECT password_hash FROM users WHERE username = $u")
        .bind(("u", username.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let user = resp.take::<Vec<UserRow>>(0).unwrap_or_default()
        .into_iter().next()
        .ok_or((StatusCode::UNAUTHORIZED, "User not found".to_string()))?;

    let valid = verify(&req.current_password, &user.password_hash)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !valid {
        return Err((StatusCode::UNAUTHORIZED, "目前密碼錯誤".to_string()));
    }

    let new_hash = hash(&req.new_password, DEFAULT_COST)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.db
        .query("UPDATE users SET password_hash = $ph WHERE username = $u")
        .bind(("ph", new_hash))
        .bind(("u", username))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

pub fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

/// Resolve the account_id (username) for a Bearer token.
/// Returns an empty string if the token is absent, invalid, or expired.
pub async fn account_id_from_token(
    db: &crate::db::SurrealDb,
    headers: &HeaderMap,
) -> String {
    #[derive(serde::Deserialize)]
    struct Row { username: String }
    let token = match extract_bearer(headers) {
        Some(t) => t,
        None => return String::new(),
    };
    let now = chrono::Utc::now().timestamp();
    db.query("SELECT username FROM sessions WHERE token = $t AND expires_at > $now LIMIT 1")
        .bind(("t", token))
        .bind(("now", now))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .map(|r| r.username)
        .unwrap_or_default()
}
