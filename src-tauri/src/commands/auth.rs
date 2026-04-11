use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use crate::state::AppState;
use crate::api_client::{daemon_get, daemon_post};

// ── Google OAuth 2.0 憑證（桌面應用程式類型）────────────────────────────────
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
    pub auth_provider: String,
}

fn default_auth_provider() -> String { "local".to_string() }

#[tauri::command]
pub async fn login(
    username: String,
    password: String,
    state: tauri::State<'_, AppState>,
) -> Result<SessionInfo, String> {
    #[derive(Deserialize)]
    struct LoginResp { token: String, username: String, expires_at: i64 }

    let resp = daemon_post::<_, LoginResp>(
        &state.http_client,
        "/auth/login",
        &serde_json::json!({"username": username, "password": password}),
        None,
    ).await.map_err(|e| format!("登入失敗：{}", e))?;

    state.set_auth_token(resp.token.clone()).await;
    state.set_username(resp.username.clone()).await;

    // 若 vault_path 非空，向 daemon 取得 vault UUID
    let vp = state.get_vault_path().await;
    if !vp.is_empty() {
        if let Ok(v) = daemon_post::<_, serde_json::Value>(
            &state.http_client,
            "/vaults",
            &serde_json::json!({"path": vp, "account_id": resp.username}),
            Some(&resp.token),
        ).await {
            if let Some(uuid) = v["vault_id"].as_str() {
                state.set_vault_uuid(uuid.to_string()).await;
            }
        }
    }

    Ok(SessionInfo { token: resp.token, username: resp.username, expires_at: resp.expires_at, auth_provider: "local".to_string() })
}

#[tauri::command]
pub async fn logout(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let token = state.get_auth_token().await;
    if !token.is_empty() {
        let _ = daemon_post::<_, serde_json::Value>(
            &state.http_client,
            "/auth/logout",
            &serde_json::json!({}),
            Some(&token),
        ).await;
    }
    state.clear_auth_token().await;
    state.set_username(String::new()).await;
    state.set_vault_uuid(String::new()).await;
    Ok(())
}

#[tauri::command]
pub async fn get_session(state: tauri::State<'_, AppState>) -> Result<Option<SessionInfo>, String> {
    let token = state.get_auth_token().await;
    if token.is_empty() {
        return Ok(None);
    }

    #[derive(Deserialize)]
    struct SessionResp { token: String, username: String, expires_at: i64 }

    match daemon_get::<Option<SessionResp>>(&state.http_client, "/auth/session", Some(&token)).await {
        Ok(Some(s)) => {
            state.set_username(s.username.clone()).await;
            Ok(Some(SessionInfo { token: s.token, username: s.username, expires_at: s.expires_at, auth_provider: "local".to_string() }))
        }
        Ok(None) => {
            state.clear_auth_token().await;
            Ok(None)
        }
        Err(_) => {
            state.clear_auth_token().await;
            Ok(None)
        }
    }
}

#[tauri::command]
pub async fn change_password(
    current_password: String,
    new_password: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let token = state.get_auth_token().await;
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/auth/change-password",
        &serde_json::json!({"current_password": current_password, "new_password": new_password}),
        Some(&token),
    ).await.map(|_| ()).map_err(|e| format!("修改密碼失敗：{}", e))
}

// ── Google OAuth 2.0（PKCE + 本地 HTTP callback）──────────────────────────

fn generate_pkce() -> (String, String) {
    let bytes: Vec<u8> = (0..4)
        .flat_map(|_| uuid::Uuid::new_v4().as_bytes().to_vec())
        .collect();
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash.as_slice());
    (verifier, challenge)
}

/// Called on app startup to restore a previously saved token into AppState.
/// Validates the token with the service; if valid, stores in AppState and returns SessionInfo.
#[tauri::command]
pub async fn restore_session(
    token: String,
    state: tauri::State<'_, AppState>,
) -> Result<Option<SessionInfo>, String> {
    if token.is_empty() {
        return Ok(None);
    }

    #[derive(Deserialize)]
    struct SessionResp { token: String, username: String, expires_at: i64, auth_provider: Option<String> }

    match daemon_get::<Option<SessionResp>>(&state.http_client, "/auth/session", Some(&token)).await {
        Ok(Some(s)) => {
            state.set_auth_token(token).await;
            state.set_username(s.username.clone()).await;
            Ok(Some(SessionInfo {
                token: s.token,
                username: s.username,
                expires_at: s.expires_at,
                auth_provider: s.auth_provider.unwrap_or_else(|| "local".to_string()),
            }))
        }
        _ => Ok(None),
    }
}

#[tauri::command]
pub async fn start_google_oauth(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<SessionInfo, String> {
    use tauri_plugin_shell::ShellExt;

    let (client_id, client_secret) = google_credentials();
    let (code_verifier, code_challenge) = generate_pkce();
    let oauth_state = uuid::Uuid::new_v4().to_string();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await.map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect_uri = format!("http://127.0.0.1:{}/callback", port);

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

    app.shell().open(auth_url.as_str(), None)
        .map_err(|e| format!("無法開啟瀏覽器: {e}"))?;

    let (mut stream, _) = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        listener.accept(),
    ).await
    .map_err(|_| "Google 登入逾時（2 分鐘）".to_string())?
    .map_err(|e| e.to_string())?;

    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read(&mut buf),
    ).await
    .map_err(|_| "讀取 callback 逾時".to_string())?
    .map_err(|e| e.to_string())?;

    let request_str = String::from_utf8_lossy(&buf[..n]);
    let request_line = request_str.lines().next().unwrap_or("");
    let raw_path = request_line.split_whitespace().nth(1).unwrap_or("");
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

    let _ = send_html_response(&mut stream, true).await;
    drop(stream);

    if returned_state.as_deref() != Some(oauth_state.as_str()) {
        return Err("OAuth state 不符，請重試".to_string());
    }
    let code = auth_code.ok_or("未收到授權碼")?;

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
        .send().await.map_err(|e| format!("Token 交換失敗: {e}"))?
        .json::<serde_json::Value>().await.map_err(|e| format!("Token 解析失敗: {e}"))?;

    let access_token = token_resp["access_token"].as_str()
        .ok_or_else(|| {
            let err = token_resp["error_description"].as_str().unwrap_or("未知錯誤");
            format!("取得 access_token 失敗: {err}")
        })?;

    let userinfo = client
        .get("https://www.googleapis.com/oauth2/v3/userinfo")
        .bearer_auth(access_token)
        .send().await.map_err(|e| format!("UserInfo 請求失敗: {e}"))?
        .json::<serde_json::Value>().await.map_err(|e| format!("UserInfo 解析失敗: {e}"))?;

    let email    = userinfo["email"].as_str().ok_or("無法取得 Email")?;
    // Use email as username (unique across Google accounts)
    let username = email.to_string();
    // Use stable Google sub as credential — never changes between sessions
    let sub = userinfo["sub"].as_str().ok_or("無法取得 Google sub")?;

    // Single-step upsert: creates or updates user + returns session token
    #[derive(Deserialize)]
    struct LoginResp { token: String, username: String, expires_at: i64 }
    let resp = daemon_post::<_, LoginResp>(
        &state.http_client,
        "/auth/google-upsert",
        &serde_json::json!({"username": username, "sub": sub}),
        None,
    ).await.map_err(|e| format!("Google 登入失敗：{e}"))?;

    state.set_auth_token(resp.token.clone()).await;
    state.set_username(resp.username.clone()).await;

    let vp = state.get_vault_path().await;
    if !vp.is_empty() {
        if let Ok(v) = daemon_post::<_, serde_json::Value>(
            &state.http_client,
            "/vaults",
            &serde_json::json!({"path": vp, "account_id": username}),
            Some(&resp.token),
        ).await {
            if let Some(uuid) = v["vault_id"].as_str() {
                state.set_vault_uuid(uuid.to_string()).await;
            }
        }
    }

    Ok(SessionInfo { token: resp.token, username: resp.username, expires_at: resp.expires_at, auth_provider: "google".to_string() })
}

// ── Google Calendar OAuth ──────────────────────────────────────────────────

/// Launch browser OAuth flow with calendar scope, exchange for refresh_token,
/// and store it in the daemon via POST /calendar/connect.
#[tauri::command]
pub async fn connect_google_calendar(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    use tauri_plugin_shell::ShellExt;

    let (client_id, client_secret) = google_credentials();
    let (code_verifier, code_challenge) = generate_pkce();
    let oauth_state = uuid::Uuid::new_v4().to_string();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await.map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect_uri = format!("http://127.0.0.1:{}/callback", port);

    let mut auth_url = url::Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
        .map_err(|e| e.to_string())?;
    auth_url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid email profile https://www.googleapis.com/auth/calendar")
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &oauth_state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent"); // force refresh_token even for returning users

    app.shell().open(auth_url.as_str(), None)
        .map_err(|e| format!("無法開啟瀏覽器: {e}"))?;

    let (mut stream, _) = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        listener.accept(),
    ).await
    .map_err(|_| "Google 授權逾時（2 分鐘）".to_string())?
    .map_err(|e| e.to_string())?;

    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read(&mut buf),
    ).await
    .map_err(|_| "讀取 callback 逾時".to_string())?
    .map_err(|e| e.to_string())?;

    let request_str = String::from_utf8_lossy(&buf[..n]);
    let request_line = request_str.lines().next().unwrap_or("");
    let raw_path = request_line.split_whitespace().nth(1).unwrap_or("");
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
                return Err(format!("Google 授權被拒絕: {v}"));
            }
            _ => {}
        }
    }

    let _ = send_html_response(&mut stream, true).await;
    drop(stream);

    if returned_state.as_deref() != Some(oauth_state.as_str()) {
        return Err("OAuth state 不符，請重試".to_string());
    }
    let code = auth_code.ok_or("未收到授權碼")?;

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
        .send().await.map_err(|e| format!("Token 交換失敗: {e}"))?
        .json::<serde_json::Value>().await.map_err(|e| format!("Token 解析失敗: {e}"))?;

    let refresh_token = token_resp["refresh_token"].as_str()
        .ok_or_else(|| {
            let err = token_resp["error_description"].as_str().unwrap_or("未收到 refresh_token");
            format!("取得 refresh_token 失敗: {err}")
        })?
        .to_string();

    // Store refresh_token in daemon
    let token = state.get_auth_token().await;
    let body = serde_json::json!({
        "refresh_token": refresh_token,
        "client_id": client_id,
        "client_secret": client_secret,
    });
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/calendar/connect",
        &body,
        Some(&token),
    ).await.map_err(|e| format!("儲存行事曆 Token 失敗: {e}"))?;

    Ok(())
}

/// Returns true if the current user has Google Calendar connected.
#[tauri::command]
pub async fn get_calendar_status(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let token = state.get_auth_token().await;
    #[derive(serde::Deserialize)]
    struct Resp { connected: bool }
    let resp = crate::api_client::daemon_get::<Resp>(
        &state.http_client,
        "/calendar/status",
        Some(&token),
    ).await.map_err(|e| format!("查詢行事曆狀態失敗: {e}"))?;
    Ok(resp.connected)
}

/// Removes the stored Google Calendar refresh_token for the current user.
#[tauri::command]
pub async fn disconnect_google_calendar(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let token = state.get_auth_token().await;
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/calendar/disconnect",
        &serde_json::json!({}),
        Some(&token),
    ).await.map_err(|e| format!("中斷行事曆連線失敗: {e}"))?;
    Ok(())
}

// ── Google Gmail OAuth ────────────────────────────────────────────────────────

/// Launch browser OAuth flow with Gmail readonly scope, exchange for refresh_token,
/// and store it in the daemon via POST /gmail/connect.
#[tauri::command]
pub async fn connect_gmail(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    use tauri_plugin_shell::ShellExt;

    let (client_id, client_secret) = google_credentials();
    let (code_verifier, code_challenge) = generate_pkce();
    let oauth_state = uuid::Uuid::new_v4().to_string();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await.map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect_uri = format!("http://127.0.0.1:{}/callback", port);

    let mut auth_url = url::Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
        .map_err(|e| e.to_string())?;
    auth_url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "https://www.googleapis.com/auth/gmail.readonly")
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &oauth_state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");

    app.shell().open(auth_url.as_str(), None)
        .map_err(|e| format!("無法開啟瀏覽器: {e}"))?;

    let (mut stream, _) = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        listener.accept(),
    ).await
    .map_err(|_| "Gmail 授權逾時（2 分鐘）".to_string())?
    .map_err(|e| e.to_string())?;

    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read(&mut buf),
    ).await
    .map_err(|_| "讀取 callback 逾時".to_string())?
    .map_err(|e| e.to_string())?;

    let request_str = String::from_utf8_lossy(&buf[..n]);
    let request_line = request_str.lines().next().unwrap_or("");
    let raw_path = request_line.split_whitespace().nth(1).unwrap_or("");
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
                return Err(format!("Gmail 授權被拒絕: {v}"));
            }
            _ => {}
        }
    }

    let _ = send_html_response(&mut stream, true).await;
    drop(stream);

    if returned_state.as_deref() != Some(oauth_state.as_str()) {
        return Err("OAuth state 不符，請重試".to_string());
    }
    let code = auth_code.ok_or("未收到授權碼")?;

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
        .send().await.map_err(|e| format!("Token 交換失敗: {e}"))?
        .json::<serde_json::Value>().await.map_err(|e| format!("Token 解析失敗: {e}"))?;

    let refresh_token = token_resp["refresh_token"].as_str()
        .ok_or_else(|| {
            let err = token_resp["error_description"].as_str().unwrap_or("未收到 refresh_token");
            format!("取得 refresh_token 失敗: {err}")
        })?
        .to_string();

    let token = state.get_auth_token().await;
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/gmail/connect",
        &serde_json::json!({ "refresh_token": refresh_token, "client_id": client_id, "client_secret": client_secret }),
        Some(&token),
    ).await.map_err(|e| format!("儲存 Gmail Token 失敗: {e}"))?;

    Ok(())
}

/// Returns true if the current user has Gmail connected.
#[tauri::command]
pub async fn get_gmail_status(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let token = state.get_auth_token().await;
    #[derive(serde::Deserialize)]
    struct Resp { connected: bool }
    let resp = crate::api_client::daemon_get::<Resp>(
        &state.http_client,
        "/gmail/status",
        Some(&token),
    ).await.map_err(|e| format!("查詢 Gmail 狀態失敗: {e}"))?;
    Ok(resp.connected)
}

/// Removes the stored Gmail refresh_token for the current user.
#[tauri::command]
pub async fn disconnect_gmail(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let token = state.get_auth_token().await;
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/gmail/disconnect",
        &serde_json::json!({}),
        Some(&token),
    ).await.map_err(|e| format!("中斷 Gmail 連線失敗: {e}"))?;
    Ok(())
}

async fn send_html_response(stream: &mut tokio::net::TcpStream, success: bool) -> std::io::Result<()> {
    let (title, msg) = if success { ("登入成功", "請返回 noteTreeLM 應用程式") } else { ("登入失敗", "請關閉此視窗並重試") };
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
