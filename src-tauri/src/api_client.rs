//! HTTP helpers for calling the daemon REST API at http://127.0.0.1:7787/api/v1

pub const DAEMON_BASE: &str = "http://127.0.0.1:7787/api/v1";

pub async fn daemon_get<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    path: &str,
    token: Option<&str>,
) -> Result<T, String> {
    let mut req = client.get(format!("{}{}", DAEMON_BASE, path));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {} {}", res.status(), res.text().await.unwrap_or_default()));
    }
    res.json::<T>().await.map_err(|e| e.to_string())
}

pub async fn daemon_post<B: serde::Serialize, T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    path: &str,
    body: &B,
    token: Option<&str>,
) -> Result<T, String> {
    let mut req = client.post(format!("{}{}", DAEMON_BASE, path)).json(body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {} {}", res.status(), res.text().await.unwrap_or_default()));
    }
    res.json::<T>().await.map_err(|e| e.to_string())
}

pub async fn daemon_put<B: serde::Serialize, T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    path: &str,
    body: &B,
    token: Option<&str>,
) -> Result<T, String> {
    let mut req = client.put(format!("{}{}", DAEMON_BASE, path)).json(body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {} {}", res.status(), res.text().await.unwrap_or_default()));
    }
    res.json::<T>().await.map_err(|e| e.to_string())
}

pub async fn daemon_patch<B: serde::Serialize, T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    path: &str,
    body: &B,
    token: Option<&str>,
) -> Result<T, String> {
    let mut req = client.patch(format!("{}{}", DAEMON_BASE, path)).json(body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {} {}", res.status(), res.text().await.unwrap_or_default()));
    }
    res.json::<T>().await.map_err(|e| e.to_string())
}

pub async fn daemon_delete<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    path: &str,
    token: Option<&str>,
) -> Result<T, String> {
    let mut req = client.delete(format!("{}{}", DAEMON_BASE, path));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {} {}", res.status(), res.text().await.unwrap_or_default()));
    }
    res.json::<T>().await.map_err(|e| e.to_string())
}

/// 讀取單一設定值（daemon GET /settings/key/:key）
pub async fn daemon_get_setting(client: &reqwest::Client, tok: Option<&str>, key: &str) -> Option<String> {
    let r: serde_json::Value = daemon_get(
        client,
        &format!("/settings/key/{}", urlencoding::encode(key)),
        tok,
    ).await.ok()?;
    match &r["value"] {
        serde_json::Value::Null => None,
        v => v.as_str().map(|s| s.to_string()),
    }
}

/// 儲存單一設定值（daemon POST /settings）
pub async fn daemon_set_setting(client: &reqwest::Client, tok: Option<&str>, key: &str, value: &str) {
    let _ = daemon_post::<_, serde_json::Value>(
        client,
        "/settings",
        &serde_json::json!({key: value}),
        tok,
    ).await;
}
