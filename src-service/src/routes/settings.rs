use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::app_state::ApiState;
use crate::routes::auth::extract_bearer;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/settings", get(get_settings).post(save_settings))
        .route("/settings/user", get(get_user_settings).post(save_user_settings))
        .route("/settings/key/:key", get(get_setting_by_key))
        .route("/settings/user/key/:key", get(get_user_setting_by_key))
        .route("/settings/api-key/:provider", get(get_api_key).post(set_api_key))
        .route("/settings/brave-key-id", post(set_brave_key_id))
        .route("/settings/user-image", get(get_user_image).post(save_user_image))
}

async fn get_settings(
    State(state): State<ApiState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    #[derive(serde::Deserialize)]
    struct Row {
        key: String,
        value: String,
    }

    let mut resp = state
        .db
        .query("SELECT `key`, `value` FROM `settings`")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Row> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let map: HashMap<String, String> = rows.into_iter().map(|r| (r.key, r.value)).collect();
    Ok(Json(json!(map)))
}

async fn save_settings(
    State(state): State<ApiState>,
    Json(data): Json<HashMap<String, Value>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    for (key, val) in data {
        let value_str = match val {
            Value::String(s) => s,
            other => other.to_string(),
        };
        // Encrypt API keys at rest; service is the single encryption point.
        let store_value = if key.starts_with("api_key_") && !value_str.is_empty() {
            let enc = crate::service::harness::crypto::encrypt_api_key_db(&state.db, &value_str).await;
            if enc.is_empty() {
                return Err((StatusCode::INTERNAL_SERVER_ERROR, "API key encryption failed".to_string()));
            }
            enc
        } else {
            value_str
        };
        upsert_setting(&state, key, store_value, now).await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn get_user_settings(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let username = get_username_from_token(&state, &headers).await?;

    #[derive(serde::Deserialize)]
    struct Row {
        key: String,
        value: String,
    }

    let mut resp = state
        .db
        .query("SELECT `key`, `value` FROM user_settings WHERE username = $u")
        .bind(("u", username))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Row> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let map: HashMap<String, String> = rows.into_iter().map(|r| (r.key, r.value)).collect();
    Ok(Json(json!(map)))
}

async fn save_user_settings(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(data): Json<HashMap<String, Value>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let username = get_username_from_token(&state, &headers).await?;
    let now = Utc::now().timestamp();

    for (key, val) in data {
        let value_str = match val {
            Value::String(s) => s,
            other => other.to_string(),
        };
        upsert_user_setting(&state, username.clone(), key, value_str, now).await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn get_setting_by_key(
    State(state): State<ApiState>,
    Path(key): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    #[derive(serde::Deserialize)]
    struct Row {
        value: String,
    }

    let mut resp = state
        .db
        .query("SELECT `value` FROM `settings` WHERE `key` = $k LIMIT 1")
        .bind(("k", key.clone()))
        .await
        .map_err(|e| {
            tracing::error!("get_setting_by_key query failed key={}: {}", key, e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    let rows: Vec<Row> = resp
        .take(0)
        .map_err(|e| {
            tracing::error!("get_setting_by_key take failed key={}: {}", key, e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    match rows.into_iter().next() {
        Some(r) => {
            // Decrypt API keys transparently before returning to caller.
            let value = if key.starts_with("api_key_") && !r.value.is_empty() {
                let plain = crate::service::harness::crypto::decrypt_api_key_db(&state.db, &r.value).await;
                if plain.is_empty() { r.value } else { plain }
            } else {
                r.value
            };
            Ok(Json(json!({ "value": value })))
        }
        None => Ok(Json(json!({ "value": null }))),
    }
}

async fn get_user_setting_by_key(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let username = get_username_from_token(&state, &headers).await?;

    #[derive(serde::Deserialize)]
    struct Row {
        value: String,
    }

    let mut resp = state
        .db
        .query("SELECT `value` FROM user_settings WHERE username = $u AND `key` = $k LIMIT 1")
        .bind(("u", username))
        .bind(("k", key))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Row> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match rows.into_iter().next() {
        Some(r) => Ok(Json(json!({ "value": r.value }))),
        None => Ok(Json(json!({ "value": null }))),
    }
}

async fn get_username_from_token(
    state: &ApiState,
    headers: &HeaderMap,
) -> Result<String, (StatusCode, String)> {
    let token = extract_bearer(headers)
        .ok_or((StatusCode::UNAUTHORIZED, "Missing token".to_string()))?;
    let now = chrono::Utc::now().timestamp();

    #[derive(serde::Deserialize)]
    struct Row {
        username: String,
    }

    let mut resp = state
        .db
        .query("SELECT username FROM sessions WHERE token = $t AND expires_at > $now")
        .bind(("t", token))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Row> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    rows.into_iter()
        .next()
        .map(|r| r.username)
        .ok_or((StatusCode::UNAUTHORIZED, "Invalid or expired token".to_string()))
}

// ── Upsert helpers ─────────────────────────────────────────────────────────────

/// Explicit SELECT + INSERT/UPDATE to avoid ON DUPLICATE KEY syntax issues with
/// SurrealDB reserved keywords (`key`, `value`, `settings`).
async fn upsert_setting(
    state: &ApiState,
    key: String,
    value: String,
    now: i64,
) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    struct IdRow { id: surrealdb::RecordId }

    let mut check = state.db
        .query("SELECT id FROM `settings` WHERE `key` = $k LIMIT 1")
        .bind(("k", key.clone()))
        .await
        .map_err(|e| e.to_string())?;
    let existing: Vec<IdRow> = check.take(0).unwrap_or_default();

    if existing.is_empty() {
        state.db
            .query("INSERT INTO `settings` (`key`, `value`, updated_at) VALUES ($k, $v, $now)")
            .bind(("k", key))
            .bind(("v", value))
            .bind(("now", now))
            .await
            .map_err(|e| e.to_string())?;
    } else {
        let id = existing.into_iter().next().unwrap().id;
        state.db
            .query("UPDATE $id SET `value` = $v, updated_at = $now")
            .bind(("id", id))
            .bind(("v", value))
            .bind(("now", now))
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn upsert_user_setting(
    state: &ApiState,
    username: String,
    key: String,
    value: String,
    now: i64,
) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    struct IdRow { id: surrealdb::RecordId }

    let mut check = state.db
        .query("SELECT id FROM user_settings WHERE username = $u AND `key` = $k LIMIT 1")
        .bind(("u", username.clone()))
        .bind(("k", key.clone()))
        .await
        .map_err(|e| e.to_string())?;
    let existing: Vec<IdRow> = check.take(0).unwrap_or_default();

    if existing.is_empty() {
        state.db
            .query("INSERT INTO user_settings (username, `key`, `value`, updated_at) VALUES ($u, $k, $v, $now)")
            .bind(("u", username))
            .bind(("k", key))
            .bind(("v", value))
            .bind(("now", now))
            .await
            .map_err(|e| e.to_string())?;
    } else {
        let id = existing.into_iter().next().unwrap().id;
        state.db
            .query("UPDATE $id SET `value` = $v, updated_at = $now")
            .bind(("id", id))
            .bind(("v", value))
            .bind(("now", now))
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── API Key endpoints ──────────────────────────────────────────────────────────

async fn get_api_key(
    State(state): State<ApiState>,
    Path(provider): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let db_key = format!("api_key_{}", provider);

    #[derive(serde::Deserialize)]
    struct Row { value: String }

    let mut resp = state.db
        .query("SELECT `value` FROM `settings` WHERE `key` = $k LIMIT 1")
        .bind(("k", db_key.clone()))
        .await
        .map_err(|e| {
            tracing::error!("get_api_key query failed key={}: {}", db_key, e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;
    let rows: Vec<Row> = resp.take(0).map_err(|e| {
        tracing::error!("get_api_key take failed key={}: {}", db_key, e);
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let key = match rows.into_iter().next().map(|r| r.value).filter(|v| !v.is_empty()) {
        Some(enc) => {
            let plain = crate::service::harness::crypto::decrypt_api_key_db(&state.db, &enc).await;
            if plain.is_empty() { None } else { Some(plain) }
        }
        None => None,
    };
    Ok(Json(json!({ "key": key })))
}

#[derive(Deserialize)]
struct SetApiKeyBody { key: String }

async fn set_api_key(
    State(state): State<ApiState>,
    Path(provider): Path<String>,
    Json(body): Json<SetApiKeyBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let db_key = format!("api_key_{}", provider);
    let now = Utc::now().timestamp();
    let encrypted = crate::service::harness::crypto::encrypt_api_key_db(&state.db, &body.key).await;
    if encrypted.is_empty() {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "API key encryption failed".to_string()));
    }
    upsert_setting(&state, db_key, encrypted, now).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({ "ok": true })))
}

// ── User image (onboarding) endpoints ────────────────────────────────────────

/// GET /settings/user-image
/// Returns user_image parsed into a field map, plus the list of known profile fields.
async fn get_user_image(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    use crate::service::harness::memory::user_image::{load_user_image, PROFILE_FIELDS};

    let username = get_username_from_token(&state, &headers).await?;
    let text = load_user_image(&state.db, &username).await;

    // Parse into field map
    let fields: std::collections::HashMap<String, String> = text.lines()
        .filter_map(|line| {
            let pos = line.find('：')?;
            let key = line[..pos].trim().to_string();
            let val = line[pos + '：'.len_utf8()..].trim().to_string();
            if key.is_empty() || val.is_empty() { return None; }
            Some((key, val))
        })
        .collect();

    Ok(Json(json!({
        "fields": fields,
        "profile_fields": PROFILE_FIELDS,
        "raw_text": text,
    })))
}

/// POST /settings/user-image
/// Body: `{ "fields": { "所在地": "台北市", "語言": "繁體中文", ... } }`
/// Writes directly to user_image (bypasses LLM — onboarding user-provided data).
async fn save_user_image(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    use crate::service::harness::memory::user_image::PROFILE_FIELDS;

    let username = get_username_from_token(&state, &headers).await?;
    let fields_obj = body.get("fields")
        .and_then(|v| v.as_object())
        .ok_or((StatusCode::BAD_REQUEST, "Missing fields object".to_string()))?;

    // Build ordered text: priority fields first, then others
    let mut lines: Vec<String> = Vec::new();
    for &pf in PROFILE_FIELDS {
        if let Some(val) = fields_obj.get(pf).and_then(|v| v.as_str()) {
            let val = val.trim();
            if !val.is_empty() { lines.push(format!("{}：{}", pf, val)); }
        }
    }
    for (key, val) in fields_obj {
        if !PROFILE_FIELDS.contains(&key.as_str()) {
            if let Some(v) = val.as_str() {
                let v = v.trim();
                if !v.is_empty() { lines.push(format!("{}：{}", key, v)); }
            }
        }
    }
    let text = lines.join("\n");

    let now = Utc::now().timestamp();
    let _ = state.db
        .query("UPSERT user_settings SET username = $u, `key` = $k, `value` = $v, updated_at = $now WHERE username = $u AND `key` = $k")
        .bind(("u", username))
        .bind(("k", "user_image".to_string()))
        .bind(("v", text))
        .bind(("now", now))
        .await;

    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct BraveKeyIdBody { key_id: String }

async fn set_brave_key_id(
    State(state): State<ApiState>,
    Json(body): Json<BraveKeyIdBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    upsert_setting(&state, "brave_key_id".to_string(), body.key_id, now).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({ "ok": true })))
}
