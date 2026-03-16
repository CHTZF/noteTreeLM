use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Manager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use crate::state::AppState;

// ── Google OAuth 2.0 憑證（桌面應用程式類型）────────────────────────────────
// 從 src-tauri/google-oauth.json 讀取（compile time embed，該檔案已 gitignore）
// 範本：src-tauri/google-oauth.example.json
const GOOGLE_OAUTH_JSON: &str = include_str!("../../google-oauth.json");

fn google_credentials() -> (String, String) {
    let v: serde_json::Value =
        serde_json::from_str(GOOGLE_OAUTH_JSON).expect("google-oauth.json 格式錯誤");
    (
        v["client_id"].as_str().expect("google-oauth.json 缺少 client_id").to_string(),
        v["client_secret"].as_str().expect("google-oauth.json 缺少 client_secret").to_string(),
    )
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SessionInfo {
    pub token: String,
    pub username: String,
    pub expires_at: i64,
    #[serde(default = "default_auth_provider")]
    pub auth_provider: String, // "local" | "google"
}

fn default_auth_provider() -> String {
    "local".to_string()
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

    let session = SessionInfo { token, username, expires_at, auth_provider: "local".to_string() };

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

// ── Google OAuth 2.0（PKCE + 本地 HTTP callback）──────────────────────────

fn generate_pkce() -> (String, String) {
    // code_verifier: 64 bytes 隨機資料，以 base64url 編碼
    let bytes: Vec<u8> = (0..4)
        .flat_map(|_| uuid::Uuid::new_v4().as_bytes().to_vec())
        .collect();
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
    // code_challenge = BASE64URL(SHA256(verifier))
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash.as_slice());
    (verifier, challenge)
}

#[tauri::command]
pub async fn start_google_oauth(app: tauri::AppHandle) -> Result<SessionInfo, String> {
    use tauri_plugin_shell::ShellExt;

    let (client_id, client_secret) = google_credentials();
    let (code_verifier, code_challenge) = generate_pkce();
    let oauth_state = uuid::Uuid::new_v4().to_string();

    // 綁定隨機 port（系統分配，避免衝突）
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect_uri = format!("http://127.0.0.1:{}/callback", port);

    // 建立 Google OAuth 授權 URL
    let mut auth_url = url::Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
        .map_err(|e| e.to_string())?;
    auth_url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid email profile")
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &oauth_state)
        .append_pair("access_type", "offline");

    // 在系統預設瀏覽器開啟 Google 登入頁
    app.shell()
        .open(auth_url.as_str(), None)
        .map_err(|e| format!("無法開啟瀏覽器: {e}"))?;

    // 等待 Google 的 redirect callback（最多 2 分鐘）
    let (mut stream, _) = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        listener.accept(),
    )
    .await
    .map_err(|_| "Google 登入逾時（2 分鐘）".to_string())?
    .map_err(|e| e.to_string())?;

    // 讀取 HTTP 請求
    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read(&mut buf),
    )
    .await
    .map_err(|_| "讀取 callback 逾時".to_string())?
    .map_err(|e| e.to_string())?;

    let request_str = String::from_utf8_lossy(&buf[..n]);
    let request_line = request_str.lines().next().unwrap_or("");
    // e.g. "GET /callback?code=xxx&state=yyy HTTP/1.1"
    let raw_path = request_line.split_whitespace().nth(1).unwrap_or("");

    // 解析 query string
    let dummy = format!("http://dummy{}", raw_path);
    let parsed_url = url::Url::parse(&dummy).map_err(|_| "解析 callback URL 失敗".to_string())?;
    let mut auth_code: Option<String> = None;
    let mut returned_state: Option<String> = None;
    for (k, v) in parsed_url.query_pairs() {
        match k.as_ref() {
            "code"  => auth_code = Some(v.into_owned()),
            "state" => returned_state = Some(v.into_owned()),
            "error" => {
                let _ = send_html_response(&mut stream, false).await;
                return Err(format!("Google 登入被拒絕: {v}"));
            }
            _ => {}
        }
    }

    // 回應瀏覽器
    let _ = send_html_response(&mut stream, true).await;
    drop(stream);

    // 驗證 state（防 CSRF）
    if returned_state.as_deref() != Some(oauth_state.as_str()) {
        return Err("OAuth state 不符，請重試".to_string());
    }
    let code = auth_code.ok_or("未收到授權碼")?;

    // 用授權碼換取 access_token
    let client = reqwest::Client::new();
    let token_resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code",          code.as_str()),
            ("client_id",     client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("redirect_uri",  redirect_uri.as_str()),
            ("grant_type",    "authorization_code"),
            ("code_verifier", code_verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("Token 交換失敗: {e}"))?
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Token 解析失敗: {e}"))?;

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| {
            let err = token_resp["error_description"].as_str().unwrap_or("未知錯誤");
            format!("取得 access_token 失敗: {err}")
        })?;

    // 取得使用者資訊
    let userinfo = client
        .get("https://www.googleapis.com/oauth2/v3/userinfo")
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| format!("UserInfo 請求失敗: {e}"))?
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("UserInfo 解析失敗: {e}"))?;

    let email = userinfo["email"].as_str().ok_or("無法取得 Email")?;
    let name  = userinfo["name"].as_str().unwrap_or(email);

    // 建立 session 並寫入磁碟
    let token      = uuid::Uuid::new_v4().to_string();
    let expires_at = chrono::Local::now().timestamp() + 30 * 24 * 3600;
    let session    = SessionInfo { token, username: name.to_string(), expires_at, auth_provider: "google".to_string() };

    let path = session_path(&app);
    let json = serde_json::to_string(&session).map_err(|e| e.to_string())?;
    tokio::fs::write(&path, json).await.map_err(|e| e.to_string())?;

    Ok(session)
}

async fn send_html_response(
    stream: &mut tokio::net::TcpStream,
    success: bool,
) -> std::io::Result<()> {
    let (title, msg) = if success {
        ("登入成功", "請返回 noteTreeLM 應用程式")
    } else {
        ("登入失敗", "請關閉此視窗並重試")
    };
    let icon = if success { "✅" } else { "❌" };
    let body = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
<style>body{{font-family:sans-serif;display:flex;align-items:center;justify-content:center;\
height:100vh;margin:0;background:#1e1e2e;color:#cdd6f4}}div{{text-align:center}}\
h2{{margin-bottom:8px}}</style></head><body><div><h2>{icon} {title}</h2>\
<p>{msg}</p></div></body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    stream.write_all(response.as_bytes()).await
}
