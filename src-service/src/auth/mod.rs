pub mod handlers;
pub mod middleware;
pub mod store;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Read,
    Write,
    Admin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceToken {
    pub device_id: String,
    pub device_name: String,
    pub refresh_token_hash: String,
    pub refresh_expires_at: i64,
    pub scope: Scope,
    pub created_at: i64,
    pub last_used_at: i64,
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    pub sub: String,   // device_id
    pub scope: Scope,
    pub exp: i64,
    pub iat: i64,
}
