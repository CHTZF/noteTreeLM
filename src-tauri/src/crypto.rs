/// API key 加密模組
///
/// 金鑰衍生：HKDF-SHA256(ikm = machine_uuid, salt = db_random_salt, info = bundle_id)
///   - machine_uuid：macOS IOPlatformUUID（硬體綁定）
///   - db_random_salt：首次啟動隨機生成，永久存於 DB settings 表（key = "crypto_salt"）
///   攻擊者需同時擁有「此台機器的 UUID」與「DB 內的 salt」才能推導出金鑰。
///
/// 密文格式：base64( nonce(12B) || AES-256-GCM(plaintext) || tag(16B) )
///
/// 使用前必須呼叫 init_encryption_key(db).await（在 app 啟動時），
/// 之後 get_encryption_key() 從 OnceLock 同步讀取，不再存取 DB。

use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    AeadCore, Aes256Gcm, Key,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use hkdf::Hkdf;
use sha2::Sha256;
use std::sync::OnceLock;

static DERIVED_KEY: OnceLock<[u8; 32]> = OnceLock::new();

// ── Machine UUID ──────────────────────────────────────────────────────────────

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
    // Windows: HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid
    // Linux:   /etc/machine-id
    "notetreelm-fallback-uuid".to_string()
}

// ── Initialisation ────────────────────────────────────────────────────────────

/// 啟動時呼叫一次（daemon 版本）：透過 daemon API 讀取/寫入 crypto_salt，其餘與 init_encryption_key 相同。
pub async fn init_encryption_key_daemon(client: &reqwest::Client, tok: Option<&str>) {
    if DERIVED_KEY.get().is_some() {
        return;
    }

    let salt_b64 = match crate::api_client::daemon_get_setting(client, tok, "crypto_salt").await {
        Some(s) if !s.is_empty() => s,
        _ => {
            use aes_gcm::aead::rand_core::RngCore;
            let mut salt = [0u8; 32];
            OsRng.fill_bytes(&mut salt);
            let encoded = B64.encode(&salt);
            crate::api_client::daemon_set_setting(client, tok, "crypto_salt", &encoded).await;
            encoded
        }
    };

    let salt_bytes = B64.decode(&salt_b64).unwrap_or_else(|_| salt_b64.into_bytes());
    let uuid = machine_uuid();
    let hk = Hkdf::<Sha256>::new(Some(&salt_bytes), uuid.as_bytes());
    let mut okm = [0u8; 32];
    hk.expand(b"com.notetreelm.app", &mut okm).expect("HKDF expand failed");
    let _ = DERIVED_KEY.set(okm);
}

/// 取得快取的加密金鑰。必須在 init_encryption_key 之後呼叫。
pub fn get_encryption_key() -> &'static [u8; 32] {
    DERIVED_KEY.get().expect("[crypto] init_encryption_key() not called before use")
}

// ── Encrypt / Decrypt ─────────────────────────────────────────────────────────

/// 加密 API key，回傳 base64(nonce || ciphertext+tag)。
/// 輸入為空直接回傳空字串。
pub fn encrypt_api_key(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    let key = Key::<Aes256Gcm>::from_slice(get_encryption_key());
    let cipher = Aes256Gcm::new(key);
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    match cipher.encrypt(&nonce, plain.as_bytes()) {
        Ok(ct) => {
            let mut combined = nonce.to_vec();
            combined.extend_from_slice(&ct);
            B64.encode(&combined)
        }
        Err(e) => {
            eprintln!("[crypto] encrypt error: {e}");
            String::new()
        }
    }
}

/// 解密 API key；解密失敗（格式錯誤或金鑰不符）回傳空字串。
pub fn decrypt_api_key(encoded: &str) -> String {
    if encoded.is_empty() {
        return String::new();
    }
    let bytes = match B64.decode(encoded) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    if bytes.len() <= 12 {
        return String::new();
    }
    let (nonce_bytes, ct) = bytes.split_at(12);
    let key = Key::<Aes256Gcm>::from_slice(get_encryption_key());
    let cipher = Aes256Gcm::new(key);
    let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
    match cipher.decrypt(nonce, ct) {
        Ok(plain) => String::from_utf8(plain).unwrap_or_default(),
        Err(_) => String::new(),
    }
}
