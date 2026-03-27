use super::{DeviceToken, Scope};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;
use chrono::Utc;
use rand::Rng;

#[derive(Clone)]
pub struct AuthStore {
    inner: Arc<RwLock<AuthStoreInner>>,
}

struct AuthStoreInner {
    jwt_secret: String,
    master_token_hash: String,
    devices: HashMap<String, DeviceToken>, // device_id -> DeviceToken
    pairing_codes: HashMap<String, i64>,   // code -> expires_at (unix ts)
}

impl AuthStore {
    pub fn new() -> Self {
        let jwt_secret = generate_random_token(32);
        let master_token = generate_random_token(24);
        let master_token_hash = bcrypt::hash(&master_token, 10).unwrap();

        // 啟動時印出 master token（之後改成存 DB + 顯示在 desktop UI）
        tracing::info!("===========================================");
        tracing::info!("Master Token: {}", master_token);
        tracing::info!("(Use this to pair new devices)");
        tracing::info!("===========================================");

        Self {
            inner: Arc::new(RwLock::new(AuthStoreInner {
                jwt_secret,
                master_token_hash,
                devices: HashMap::new(),
                pairing_codes: HashMap::new(),
            })),
        }
    }

    /// Generate a short-lived one-time pairing code (valid 5 minutes).
    pub async fn generate_pairing_code(&self) -> String {
        let code = generate_random_token(6); // 12 hex chars
        let expires_at = Utc::now().timestamp() + 300; // 5 min
        let mut inner = self.inner.write().await;
        // Clean up expired codes
        inner.pairing_codes.retain(|_, &mut exp| exp > Utc::now().timestamp());
        inner.pairing_codes.insert(code.clone(), expires_at);
        code
    }

    /// Consume a pairing code (one-time use). Returns true if valid and not expired.
    pub async fn consume_pairing_code(&self, code: &str) -> bool {
        let mut inner = self.inner.write().await;
        let now = Utc::now().timestamp();
        if let Some(&expires_at) = inner.pairing_codes.get(code) {
            inner.pairing_codes.remove(code);
            expires_at > now
        } else {
            false
        }
    }

    pub async fn verify_master_token(&self, token: &str) -> bool {
        let inner = self.inner.read().await;
        bcrypt::verify(token, &inner.master_token_hash).unwrap_or(false)
    }

    pub async fn create_device_token(
        &self,
        device_name: String,
        scope: Scope,
    ) -> (String, String) {
        // access token（JWT）
        let device_id = Uuid::new_v4().to_string();
        let access_token = self.mint_access_token(&device_id, scope.clone()).await;

        // refresh token（opaque random）
        let refresh_token = generate_random_token(32);
        let refresh_token_hash = bcrypt::hash(&refresh_token, 10).unwrap();
        let refresh_expires_at = Utc::now().timestamp() + 30 * 24 * 3600;

        let device = DeviceToken {
            device_id: device_id.clone(),
            device_name,
            refresh_token_hash,
            refresh_expires_at,
            scope,
            created_at: Utc::now().timestamp(),
            last_used_at: Utc::now().timestamp(),
            revoked: false,
        };

        let mut inner = self.inner.write().await;
        inner.devices.insert(device_id, device);

        (access_token, refresh_token)
    }

    pub async fn mint_access_token(&self, device_id: &str, scope: Scope) -> String {
        use jsonwebtoken::{encode, EncodingKey, Header};
        let inner = self.inner.read().await;
        let now = Utc::now().timestamp();
        let claims = super::JwtClaims {
            sub: device_id.to_string(),
            scope,
            exp: now + 3600,
            iat: now,
        };
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(inner.jwt_secret.as_bytes()),
        ).unwrap()
    }

    pub async fn verify_access_token(&self, token: &str) -> Option<super::JwtClaims> {
        use jsonwebtoken::{decode, DecodingKey, Validation};
        let inner = self.inner.read().await;
        decode::<super::JwtClaims>(
            token,
            &DecodingKey::from_secret(inner.jwt_secret.as_bytes()),
            &Validation::default(),
        ).ok().map(|d| d.claims)
    }

    /// 用 refresh token 換新的 access + refresh token（token rotation）
    pub async fn refresh(&self, refresh_token: &str) -> Option<(String, String)> {
        let device_id = {
            let inner = self.inner.read().await;
            let now = Utc::now().timestamp();
            inner.devices.iter()
                .find(|(_, d)| {
                    !d.revoked
                        && d.refresh_expires_at > now
                        && bcrypt::verify(refresh_token, &d.refresh_token_hash).unwrap_or(false)
                })
                .map(|(id, _)| id.clone())?
        };

        let scope = {
            let inner = self.inner.read().await;
            inner.devices.get(&device_id)?.scope.clone()
        };

        // 輪換：撤銷舊 refresh token，發新的
        let new_refresh = generate_random_token(32);
        let new_hash = bcrypt::hash(&new_refresh, 10).unwrap();
        let new_exp = Utc::now().timestamp() + 30 * 24 * 3600;

        {
            let mut inner = self.inner.write().await;
            if let Some(d) = inner.devices.get_mut(&device_id) {
                d.refresh_token_hash = new_hash;
                d.refresh_expires_at = new_exp;
                d.last_used_at = Utc::now().timestamp();
            }
        }

        let access = self.mint_access_token(&device_id, scope).await;
        Some((access, new_refresh))
    }

    pub async fn revoke(&self, refresh_token: &str) -> bool {
        let mut inner = self.inner.write().await;
        for device in inner.devices.values_mut() {
            if bcrypt::verify(refresh_token, &device.refresh_token_hash).unwrap_or(false) {
                device.revoked = true;
                return true;
            }
        }
        false
    }

    pub async fn list_devices(&self) -> Vec<DeviceToken> {
        let inner = self.inner.read().await;
        inner.devices.values().filter(|d| !d.revoked).cloned().collect()
    }

    pub async fn revoke_by_device_id(&self, device_id: &str) -> bool {
        let mut inner = self.inner.write().await;
        if let Some(d) = inner.devices.get_mut(device_id) {
            d.revoked = true;
            return true;
        }
        false
    }
}

fn generate_random_token(bytes: usize) -> String {
    let random_bytes: Vec<u8> = (0..bytes).map(|_| rand::thread_rng().gen()).collect();
    hex::encode(random_bytes)
}
