use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use super::{store::AuthStore, Scope};

#[derive(Deserialize)]
pub struct PairRequest {
    /// Permanent master token OR short-lived pairing code (one of the two is required)
    pub master_token: Option<String>,
    pub pairing_code: Option<String>,
    pub device_name: String,
    #[serde(default = "default_scope")]
    pub scope: Scope,
}

fn default_scope() -> Scope { Scope::Write }

#[derive(Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u32,
    pub device_id: Option<String>,
}

#[derive(Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Deserialize)]
pub struct RevokeRequest {
    pub refresh_token: String,
}

pub async fn pair(
    State(store): State<AuthStore>,
    Json(req): Json<PairRequest>,
) -> Result<Json<TokenResponse>, StatusCode> {
    let authorized = match (&req.master_token, &req.pairing_code) {
        (Some(tok), _) => store.verify_master_token(tok).await,
        (_, Some(code)) => store.consume_pairing_code(code).await,
        _ => false,
    };
    if !authorized {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (access, refresh) = store.create_device_token(req.device_name, req.scope).await;
    Ok(Json(TokenResponse {
        access_token: access,
        refresh_token: refresh,
        expires_in: 3600,
        device_id: None,
    }))
}

pub async fn generate_pairing_code(
    State(store): State<AuthStore>,
) -> Json<serde_json::Value> {
    let code = store.generate_pairing_code().await;
    Json(serde_json::json!({
        "pairing_code": code,
        "expires_in": 300,
    }))
}

pub async fn refresh(
    State(store): State<AuthStore>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<TokenResponse>, StatusCode> {
    match store.refresh(&req.refresh_token).await {
        Some((access, refresh)) => Ok(Json(TokenResponse {
            access_token: access,
            refresh_token: refresh,
            expires_in: 3600,
            device_id: None,
        })),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

pub async fn revoke(
    State(store): State<AuthStore>,
    Json(req): Json<RevokeRequest>,
) -> Result<StatusCode, StatusCode> {
    if store.revoke(&req.refresh_token).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

#[derive(Serialize)]
pub struct DeviceInfo {
    pub device_id: String,
    pub device_name: String,
    pub scope: Scope,
    pub created_at: i64,
    pub last_used_at: i64,
}

pub async fn list_devices(
    State(store): State<AuthStore>,
) -> Json<Vec<DeviceInfo>> {
    let devices = store.list_devices().await;
    Json(devices.into_iter().map(|d| DeviceInfo {
        device_id: d.device_id,
        device_name: d.device_name,
        scope: d.scope,
        created_at: d.created_at,
        last_used_at: d.last_used_at,
    }).collect())
}

pub async fn revoke_device(
    State(store): State<AuthStore>,
    Path(device_id): Path<String>,
) -> StatusCode {
    if store.revoke_by_device_id(&device_id).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}
