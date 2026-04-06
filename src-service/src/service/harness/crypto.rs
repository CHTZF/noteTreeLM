//! API key decryption for service-side use.
//!
//! Mirrors the Tauri-side crypto module but reads `crypto_salt` directly from
//! the SurrealDB `settings` table instead of going through the daemon HTTP API.
//! The derived key is identical as long as the same machine UUID and salt are used.
//!
//! Encryption scheme: AES-256-GCM, key = HKDF-SHA256(ikm=machine_uuid, salt=crypto_salt, info="com.notetreelm.app")
//! Ciphertext format: base64( nonce(12B) || ciphertext+tag )

use aes_gcm::{aead::{Aead, KeyInit}, Aes256Gcm, Key};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use hkdf::Hkdf;
use sha2::Sha256;

#[cfg(target_os = "macos")]
fn machine_uuid() -> String {
    let out = match std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return "notetreelm-fallback-uuid".to_string(),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if line.contains("IOPlatformUUID") {
            if let Some(start) = line.rfind('"') {
                if let Some(end) = line[..start].rfind('"') {
                    let uuid = &line[end + 1..start];
                    if !uuid.is_empty() {
                        return uuid.to_string();
                    }
                }
            }
        }
    }
    "notetreelm-fallback-uuid".to_string()
}

#[cfg(not(target_os = "macos"))]
fn machine_uuid() -> String {
    "notetreelm-fallback-uuid".to_string()
}

/// Decrypt an API key that was encrypted by the Tauri app.
/// Reads `crypto_salt` from the `settings` table to derive the same AES-GCM key.
/// Returns an empty string on any failure (missing salt, bad ciphertext, etc.).
pub(crate) async fn decrypt_api_key_db(db: &crate::db::SurrealDb, encrypted: &str) -> String {
    if encrypted.is_empty() {
        return String::new();
    }

    #[derive(serde::Deserialize)]
    struct Row { value: String }

    let salt_b64: String = db
        .query("SELECT `value` FROM `settings` WHERE `key` = 'crypto_salt' LIMIT 1")
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .map(|r| r.value)
        .unwrap_or_default();

    if salt_b64.is_empty() {
        return String::new();
    }

    let salt_bytes = B64.decode(&salt_b64).unwrap_or_else(|_| salt_b64.into_bytes());
    let uuid = machine_uuid();
    let hk = Hkdf::<Sha256>::new(Some(&salt_bytes), uuid.as_bytes());
    let mut okm = [0u8; 32];
    if hk.expand(b"com.notetreelm.app", &mut okm).is_err() {
        return String::new();
    }

    let bytes = match B64.decode(encrypted) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    if bytes.len() <= 12 {
        return String::new();
    }
    let (nonce_bytes, ct) = bytes.split_at(12);
    let key = Key::<Aes256Gcm>::from_slice(&okm);
    let cipher = Aes256Gcm::new(key);
    let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
    match cipher.decrypt(nonce, ct) {
        Ok(plain) => String::from_utf8(plain).unwrap_or_default(),
        Err(_) => String::new(),
    }
}
