//! Google Calendar integration tools.
//!
//! # Token storage (per user in `user_settings`)
//! - `google_calendar_refresh_token` — long-lived, encrypted at rest via crypto module
//!
//! # App-level config (global `settings`)
//! - `google_client_id` — Google OAuth client ID (same one used by the Tauri frontend)
//!
//! # Token refresh
//! Google desktop-app PKCE flows allow refreshing with just `client_id` (no secret).
//! We exchange the stored refresh_token for a short-lived access_token on every call.
//!
//! # Frontend contract
//! After completing Google OAuth with `calendar` scope, the frontend must POST the
//! refresh_token to `POST /calendar/connect` so the backend can store it.

use serde_json::{json, Value};
use crate::db::SurrealDb;

const REFRESH_TOKEN_KEY:  &str = "google_calendar_refresh_token";
const CLIENT_ID_KEY:      &str = "google_client_id";
const CLIENT_SECRET_KEY:  &str = "google_client_secret";
const GOOGLE_TOKEN_URL:   &str = "https://oauth2.googleapis.com/token";
const CALENDAR_API_BASE:  &str = "https://www.googleapis.com/calendar/v3";

// ── Token helpers ─────────────────────────────────────────────────────────────

async fn load_refresh_token(db: &SurrealDb, account_id: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Row { value: String }
    let mut r = db
        .query("SELECT `value` FROM user_settings WHERE username = $u AND `key` = $k LIMIT 1")
        .bind(("u", account_id.to_string()))
        .bind(("k", REFRESH_TOKEN_KEY.to_string()))
        .await.ok()?;
    let rows: Vec<Row> = r.take(0).ok()?;
    let enc = rows.into_iter().next().map(|r| r.value).filter(|v| !v.is_empty())?;
    let plain = crate::service::harness::crypto::decrypt_api_key_db(db, &enc).await;
    if plain.is_empty() { None } else { Some(plain) }
}

async fn load_setting(db: &SurrealDb, key: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Row { value: String }
    let mut r = db
        .query("SELECT `value` FROM `settings` WHERE `key` = $k LIMIT 1")
        .bind(("k", key.to_string()))
        .await.ok()?;
    let rows: Vec<Row> = r.take(0).ok()?;
    rows.into_iter().next().map(|r| r.value).filter(|v| !v.is_empty())
}

/// Exchange a refresh_token for a short-lived access_token.
/// Returns `None` if any step fails (missing credentials, network error, Google error).
pub(crate) async fn get_access_token(
    client:     &reqwest::Client,
    db:         &SurrealDb,
    account_id: &str,
) -> Option<String> {
    let refresh_token = load_refresh_token(db, account_id).await?;
    let client_id     = load_setting(db, CLIENT_ID_KEY).await?;
    let client_secret = load_setting(db, CLIENT_SECRET_KEY).await.unwrap_or_default();

    let mut params = vec![
        ("client_id",     client_id.as_str()),
        ("grant_type",    "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
    ];
    if !client_secret.is_empty() {
        params.push(("client_secret", client_secret.as_str()));
    }

    let resp: Value = client
        .post(GOOGLE_TOKEN_URL)
        .form(&params)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    if let Some(err) = resp.get("error") {
        tracing::warn!("[calendar] token exchange error: {}", err);
        return None;
    }
    resp["access_token"].as_str().map(String::from)
}

// ── Public tool functions ─────────────────────────────────────────────────────

/// Fetch calendar events for the given RFC 3339 time range.
/// Returns a simplified list: { title, start, end, location, description }.
pub(crate) async fn calendar_get_events(
    client:     &reqwest::Client,
    db:         &SurrealDb,
    account_id: &str,
    time_min:   &str, // RFC 3339, e.g. "2026-04-10T00:00:00+08:00"
    time_max:   &str,
    max_results: u32,
) -> Result<Value, String> {
    let access_token = get_access_token(client, db, account_id).await
        .ok_or_else(|| "Google Calendar 未連接或 token 無效。請先在設定中連接 Google Calendar。".to_string())?;

    let url = format!("{}/calendars/primary/events", CALENDAR_API_BASE);
    let resp = client
        .get(&url)
        .bearer_auth(&access_token)
        .query(&[
            ("timeMin",    time_min),
            ("timeMax",    time_max),
            ("singleEvents", "true"),
            ("orderBy",    "startTime"),
            ("maxResults", &max_results.to_string()),
        ])
        .send()
        .await
        .map_err(|e| format!("Calendar API 請求失敗：{}", e))?;

    let body: Value = resp.json().await
        .map_err(|e| format!("Calendar API 回應解析失敗：{}", e))?;

    if let Some(err) = body.get("error") {
        return Err(format!("Google Calendar 錯誤：{}", err));
    }

    let events: Vec<Value> = body["items"]
        .as_array()
        .map(|items| items.iter().map(|item| {
            let start = item["start"]["dateTime"]
                .as_str()
                .or_else(|| item["start"]["date"].as_str())
                .unwrap_or("");
            let end = item["end"]["dateTime"]
                .as_str()
                .or_else(|| item["end"]["date"].as_str())
                .unwrap_or("");
            json!({
                "title":       item["summary"].as_str().unwrap_or("(無標題)"),
                "start":       start,
                "end":         end,
                "location":    item["location"].as_str().unwrap_or(""),
                "description": item["description"].as_str().unwrap_or(""),
                "html_link":   item["htmlLink"].as_str().unwrap_or(""),
            })
        }).collect())
        .unwrap_or_default();

    Ok(json!({ "events": events, "count": events.len() }))
}

/// Create a new calendar event on the user's primary calendar.
pub(crate) async fn calendar_create_event(
    client:      &reqwest::Client,
    db:          &SurrealDb,
    account_id:  &str,
    title:       &str,
    start:       &str, // RFC 3339 with timezone, e.g. "2026-04-10T14:00:00+08:00"
    end:         &str,
    description: &str,
    location:    &str,
) -> Result<Value, String> {
    let access_token = get_access_token(client, db, account_id).await
        .ok_or_else(|| "Google Calendar 未連接或 token 無效。請先在設定中連接 Google Calendar。".to_string())?;

    let body = json!({
        "summary":     title,
        "description": description,
        "location":    location,
        "start":       { "dateTime": start },
        "end":         { "dateTime": end },
    });

    let url = format!("{}/calendars/primary/events", CALENDAR_API_BASE);
    let resp = client
        .post(&url)
        .bearer_auth(&access_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Calendar API 請求失敗：{}", e))?;

    let result: Value = resp.json().await
        .map_err(|e| format!("Calendar API 回應解析失敗：{}", e))?;

    if let Some(err) = result.get("error") {
        return Err(format!("Google Calendar 錯誤：{}", err));
    }

    Ok(json!({
        "ok":       true,
        "title":    result["summary"].as_str().unwrap_or(title),
        "start":    result["start"]["dateTime"].as_str().unwrap_or(start),
        "end":      result["end"]["dateTime"].as_str().unwrap_or(end),
        "html_link": result["htmlLink"].as_str().unwrap_or(""),
    }))
}

/// Store an encrypted refresh_token in user_settings.
/// Called from the `/calendar/connect` route after frontend completes OAuth.
pub(crate) async fn store_refresh_token(
    db:            &SurrealDb,
    account_id:    &str,
    refresh_token: &str,
) -> bool {
    let enc = crate::service::harness::crypto::encrypt_api_key_db(db, refresh_token).await;
    if enc.is_empty() { return false; }

    let now = chrono::Utc::now().timestamp();
    let _ = db
        .query("UPSERT user_settings SET username = $u, `key` = $k, `value` = $v, updated_at = $now \
                WHERE username = $u AND `key` = $k")
        .bind(("u", account_id.to_string()))
        .bind(("k", REFRESH_TOKEN_KEY.to_string()))
        .bind(("v", enc))
        .bind(("now", now))
        .await;
    true
}
