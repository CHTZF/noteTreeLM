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

use crate::api_state::ApiState;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
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

    let valid = verify(&req.password, &user.password_hash)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !valid {
        return Err((StatusCode::UNAUTHORIZED, "Invalid credentials".to_string()));
    }

    let token = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    let expires_at = now + 86400 * 30; // 30 days

    state
        .db
        .query("INSERT INTO sessions (token, username, expires_at, auth_provider, created_at) VALUES ($token, $username, $expires_at, 'local', $now)")
        .bind(("token", token.clone()))
        .bind(("username", user.username.clone()))
        .bind(("expires_at", expires_at))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

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
    }

    let now = Utc::now().timestamp();
    let mut resp = state
        .db
        .query("SELECT token, username, expires_at FROM sessions WHERE token = $t AND expires_at > $now")
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

    let password_hash = hash(&req.password, DEFAULT_COST)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO users (username, password_hash, created_at) VALUES ($u, $ph, $now)")
        .bind(("u", req.username.clone()))
        .bind(("ph", password_hash))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true, "username": req.username })))
}

pub fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}
