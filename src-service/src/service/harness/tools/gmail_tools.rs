//! Google Gmail integration tools.
//!
//! # Token storage (per user in `user_settings`)
//! - `google_gmail_refresh_token` — long-lived, encrypted at rest via crypto module
//!
//! # Scopes
//! `https://www.googleapis.com/auth/gmail.readonly` — read-only access
//!
//! # Security
//! All email body content is wrapped via `security::wrap_external_content` before
//! being returned to the LLM, guarding against prompt injection in email bodies.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use serde_json::{json, Value};
use crate::db::SurrealDb;
use crate::service::harness::security::wrap_external_content;

const REFRESH_TOKEN_KEY:  &str = "google_gmail_refresh_token";
const CLIENT_ID_KEY:      &str = "google_client_id";
const CLIENT_SECRET_KEY:  &str = "google_client_secret";
const GOOGLE_TOKEN_URL:   &str = "https://oauth2.googleapis.com/token";
const GMAIL_API_BASE:     &str = "https://gmail.googleapis.com/gmail/v1/users/me";

// ── Access token in-memory cache ──────────────────────────────────────────────
// Google access tokens are valid for 3600s. Caching avoids a round-trip to
// oauth2.googleapis.com on every tool call within the same process lifetime.

struct CachedToken {
    token:      String,
    expires_at: i64, // Unix timestamp
}

static TOKEN_CACHE: OnceLock<Mutex<HashMap<String, CachedToken>>> = OnceLock::new();

fn token_cache() -> &'static Mutex<HashMap<String, CachedToken>> {
    TOKEN_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

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

/// Exchange a refresh_token for a short-lived access_token, with in-memory caching.
/// Tokens are cached per account_id and reused until 60s before expiry.
pub(crate) async fn get_access_token(
    client:     &reqwest::Client,
    db:         &SurrealDb,
    account_id: &str,
) -> Option<String> {
    let now = chrono::Utc::now().timestamp();
    // Check cache first (hold lock briefly, then drop before any await)
    {
        if let Ok(cache) = token_cache().lock() {
            if let Some(cached) = cache.get(account_id) {
                if cached.expires_at > now + 60 {
                    return Some(cached.token.clone());
                }
            }
        }
    }

    // Cache miss or near-expiry — exchange refresh_token for new access_token
    let refresh_token = load_refresh_token(db, account_id).await.or_else(|| {
        tracing::warn!("[gmail] no refresh_token found for account_id={:?}", account_id);
        None
    })?;
    let client_id = load_setting(db, CLIENT_ID_KEY).await.or_else(|| {
        tracing::warn!("[gmail] no google_client_id found in global settings");
        None
    })?;
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
        .send().await.ok()?
        .json().await.ok()?;

    if let Some(err) = resp.get("error") {
        tracing::warn!("[gmail] token exchange error: {}", err);
        return None;
    }
    let token = resp["access_token"].as_str().map(String::from)?;
    let expires_in = resp["expires_in"].as_i64().unwrap_or(3600);

    // Store in cache
    if let Ok(mut cache) = token_cache().lock() {
        cache.insert(account_id.to_string(), CachedToken {
            token: token.clone(),
            expires_at: now + expires_in,
        });
    }
    Some(token)
}

// ── Public tool functions ─────────────────────────────────────────────────────

/// List emails matching a Gmail query (returns subject/from/date/snippet per message).
/// Does NOT return full body — use `gmail_get_message` for that.
pub(crate) async fn gmail_list_messages(
    client:      &reqwest::Client,
    db:          &SurrealDb,
    account_id:  &str,
    query:       &str,
    max_results: u32,
) -> Result<Value, String> {
    let access_token = get_access_token(client, db, account_id).await
        .ok_or("Gmail 未連接或 token 無效。請先在設定中連接 Gmail。")?;

    // Step 1: Get message IDs
    let list_url = format!("{}/messages", GMAIL_API_BASE);
    let list_resp: Value = client
        .get(&list_url)
        .bearer_auth(&access_token)
        .query(&[
            ("q", query),
            ("maxResults", &max_results.to_string()),
        ])
        .send().await.map_err(|e| format!("Gmail API 失敗: {e}"))?
        .json().await.map_err(|e| format!("Gmail 回應解析失敗: {e}"))?;

    if let Some(err) = list_resp.get("error") {
        return Err(format!("Gmail 錯誤: {}", err));
    }

    let ids: Vec<String> = list_resp["messages"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())
        .unwrap_or_default();

    if ids.is_empty() {
        return Ok(json!({ "messages": [], "count": 0 }));
    }

    // Step 2: Batch-fetch metadata in a single HTTP round-trip via Gmail Batch API.
    // Each sub-request is a GET for one message's metadata headers.
    // Gmail batch endpoint: POST https://www.googleapis.com/batch/gmail/v1
    // Content-Type: multipart/mixed; boundary=<boundary>
    let fetch_ids: Vec<&String> = ids.iter().take(max_results as usize).collect();
    let boundary = "gmail_batch_boundary";
    let mut body = String::new();
    for id in &fetch_ids {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Type: application/http\r\n\r\n\
             GET /gmail/v1/users/me/messages/{id}?format=metadata\
             &metadataHeaders=Subject&metadataHeaders=From&metadataHeaders=Date\r\n\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));

    let batch_resp = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        client
            .post("https://www.googleapis.com/batch/gmail/v1")
            .bearer_auth(&access_token)
            .header("Content-Type", format!("multipart/mixed; boundary={boundary}"))
            .body(body)
            .send(),
    ).await
    .map_err(|_| "Gmail batch request timed out".to_string())?
    .map_err(|e| format!("Gmail batch 失敗: {e}"))?;

    let batch_text = batch_resp.text().await
        .map_err(|e| format!("Gmail batch 回應讀取失敗: {e}"))?;

    // Parse multipart response: each part contains HTTP status + JSON body
    let messages: Vec<Value> = parse_batch_response(&batch_text, &fetch_ids);
    let count = messages.len();
    Ok(json!({ "messages": messages, "count": count }))
}

/// Read the full body of a single email by message ID.
/// The returned content is wrapped in injection-prevention markers.
pub(crate) async fn gmail_get_message(
    client:     &reqwest::Client,
    db:         &SurrealDb,
    account_id: &str,
    message_id: &str,
) -> Result<Value, String> {
    let access_token = get_access_token(client, db, account_id).await
        .ok_or("Gmail 未連接或 token 無效。請先在設定中連接 Gmail。")?;

    let msg_url = format!("{}/messages/{}", GMAIL_API_BASE, message_id);
    let msg: Value = client
        .get(&msg_url)
        .bearer_auth(&access_token)
        .query(&[("format", "full")])
        .send().await.map_err(|e| format!("Gmail API 失敗: {e}"))?
        .json().await.map_err(|e| format!("Gmail 回應解析失敗: {e}"))?;

    if let Some(err) = msg.get("error") {
        return Err(format!("Gmail 錯誤: {}", err));
    }

    // Extract headers
    let headers = msg["payload"]["headers"].as_array()
        .cloned()
        .unwrap_or_default();
    let get_header = |name: &str| -> String {
        headers.iter()
            .find(|h| h["name"].as_str().map(|n| n.eq_ignore_ascii_case(name)).unwrap_or(false))
            .and_then(|h| h["value"].as_str())
            .unwrap_or("")
            .to_string()
    };
    let subject = get_header("Subject");
    let from    = get_header("From");
    let date    = get_header("Date");

    // Extract body (prefer text/plain, fallback text/html → strip tags)
    let body_raw = extract_body(&msg["payload"]);
    let body = if body_raw.len() > 6000 {
        let truncated: String = body_raw.chars().take(6000).collect();
        format!("{}\n…（已截斷至 6000 字元）", truncated)
    } else {
        body_raw
    };

    // Wrap in security boundary
    let source = format!("Gmail/{}", message_id);
    let email_text = format!("寄件者: {from}\n日期: {date}\n主旨: {subject}\n\n{body}");
    let wrapped = wrap_external_content(&source, &email_text);

    Ok(json!({
        "id":      message_id,
        "subject": subject,
        "from":    from,
        "date":    date,
        "body":    wrapped,
    }))
}

/// Store an encrypted refresh_token in user_settings.
pub(crate) async fn store_gmail_refresh_token(
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

// ── Gmail Batch API response parser ──────────────────────────────────────────

/// Parse a Gmail Batch API multipart/mixed response.
/// Each part contains an HTTP status line + headers + JSON body.
/// We extract the JSON body from 200-OK parts and build message summaries.
fn parse_batch_response(body: &str, ids: &[&String]) -> Vec<Value> {
    let mut messages = Vec::new();
    // Split on MIME boundary lines (lines starting with "--")
    // Each part looks like:
    //   --<boundary>
    //   Content-Type: application/http
    //   ...
    //   HTTP/1.1 200 OK
    //   Content-Type: application/json; ...
    //
    //   { ...json... }
    let mut part_start = 0;
    let lines: Vec<&str> = body.lines().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in &lines {
        if line.starts_with("--") {
            if !current.trim().is_empty() {
                parts.push(current.clone());
            }
            current = String::new();
        } else {
            current.push_str(line);
            current.push('\n');
        }
        let _ = part_start; // suppress unused warning
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }

    for part in &parts {
        // Find the JSON object in the part (starts with '{')
        if let Some(json_start) = part.find('{') {
            let json_str = &part[json_start..];
            // Find matching closing brace by taking the last '}'
            if let Some(json_end) = json_str.rfind('}') {
                let json_str = &json_str[..=json_end];
                if let Ok(msg) = serde_json::from_str::<Value>(json_str) {
                    // Skip error responses
                    if msg.get("error").is_some() { continue; }
                    let id = msg["id"].as_str().unwrap_or("").to_string();
                    if id.is_empty() { continue; }
                    // Only include IDs we actually requested
                    if !ids.iter().any(|r| r.as_str() == id) { continue; }
                    let headers = msg["payload"]["headers"].as_array()
                        .cloned()
                        .unwrap_or_default();
                    let get_header = |name: &str| -> String {
                        headers.iter()
                            .find(|h| h["name"].as_str()
                                .map(|n| n.eq_ignore_ascii_case(name))
                                .unwrap_or(false))
                            .and_then(|h| h["value"].as_str())
                            .unwrap_or("")
                            .to_string()
                    };
                    messages.push(json!({
                        "id":      id,
                        "subject": get_header("Subject"),
                        "from":    get_header("From"),
                        "date":    get_header("Date"),
                    }));
                }
            }
        }
    }

    // Preserve original order (batch response order may differ)
    messages.sort_by_key(|m| {
        ids.iter().position(|id| id.as_str() == m["id"].as_str().unwrap_or(""))
            .unwrap_or(usize::MAX)
    });
    messages
}

// ── Body extraction helpers ───────────────────────────────────────────────────

fn extract_body(payload: &Value) -> String {
    // Try direct body data first
    if let Some(data) = payload["body"]["data"].as_str() {
        if !data.is_empty() {
            if let Ok(bytes) = base64_url_decode(data) {
                if let Ok(text) = String::from_utf8(bytes) {
                    let mime = payload["mimeType"].as_str().unwrap_or("");
                    return if mime.contains("html") { strip_html(&text) } else { text };
                }
            }
        }
    }

    // Walk multipart parts — prefer text/plain when substantial, else text/html.
    // Marketing emails often have a useless `text/plain` ("View in browser…") alongside
    // a rich `text/html` body.  Only use plain text if it is at least 200 chars;
    // otherwise fall through to the HTML fallback so the real content is returned.
    if let Some(parts) = payload["parts"].as_array() {
        let mut plain_text   = String::new();
        let mut html_fallback = String::new();
        for part in parts {
            let mime = part["mimeType"].as_str().unwrap_or("");
            if mime == "text/plain" {
                let text = extract_body(part);
                if text.len() > plain_text.len() { plain_text = text; }
            } else if mime == "text/html" {
                let text = extract_body(part);
                if text.len() > html_fallback.len() { html_fallback = text; }
            } else if mime.starts_with("multipart/") {
                // Recurse into nested multipart — treat result as plain candidate
                let text = extract_body(part);
                if text.len() > plain_text.len() { plain_text = text; }
            }
        }
        // Prefer plain text only when it is substantial (≥ 200 chars).
        // A short plain text like "View this email in your browser" is useless —
        // fall through to the HTML-stripped version instead.
        if plain_text.len() >= 200 {
            return plain_text;
        } else if !html_fallback.is_empty() {
            return html_fallback;
        } else if !plain_text.is_empty() {
            return plain_text;  // short but it's all we have
        }
    }

    String::new()
}

fn base64_url_decode(s: &str) -> Result<Vec<u8>, ()> {
    use base64::Engine as _;
    // Gmail uses URL-safe base64 without padding
    let padded = match s.len() % 4 {
        2 => format!("{}==", s),
        3 => format!("{}=", s),
        _ => s.to_string(),
    };
    base64::engine::general_purpose::URL_SAFE
        .decode(padded)
        .map_err(|_| ())
}

fn strip_html(html: &str) -> String {
    // Step 1: remove <style>...</style> and <script>...</script> blocks entirely
    // (not just the tags — the CSS/JS content is noise for the LLM).
    let html = remove_block_elements(html, "style");
    let html = remove_block_elements(&html, "script");

    // Step 2: replace common block-level tags with newlines so structure is preserved
    let html = replace_block_tags_with_newlines(&html);

    // Step 3: strip remaining tags character-by-character
    let mut result = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => { in_tag = false; }
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }

    // Step 4: decode common HTML entities
    let result = decode_html_entities(&result);

    // Step 5: collapse whitespace / blank lines
    result.lines()
        .map(str::trim)
        .fold(String::new(), |mut acc, line| {
            if line.is_empty() && acc.ends_with('\n') {
                // skip extra blank lines
            } else {
                acc.push_str(line);
                acc.push('\n');
            }
            acc
        })
        .trim()
        .to_string()
}

/// Remove all occurrences of `<tag ...>...</tag>` (case-insensitive, non-greedy).
fn remove_block_elements(html: &str, tag: &str) -> String {
    let open_pat  = format!("<{}", tag);
    let close_pat = format!("</{}>", tag);
    let close_pat_uc = format!("</{}>", tag.to_uppercase());
    let mut result = String::new();
    let mut rest = html;
    loop {
        // Find the opening tag (case-insensitive: check both lower and upper)
        let start_lower = rest.to_lowercase().find(&open_pat);
        let start = match start_lower {
            None => break,
            Some(s) => s,
        };
        result.push_str(&rest[..start]);
        let after_open = &rest[start..];
        // Find closing tag
        let close_lower = after_open.to_lowercase().find(&close_pat);
        let close_uc = after_open.to_lowercase().find(&close_pat_uc);
        let end = close_lower.or(close_uc);
        match end {
            Some(pos) => {
                // skip past the closing tag
                rest = &after_open[pos + close_pat.len()..];
            }
            None => {
                // No closing tag — skip to end
                break;
            }
        }
    }
    result.push_str(rest);
    result
}

/// Replace block-level HTML tags with newlines to preserve paragraph structure.
fn replace_block_tags_with_newlines(html: &str) -> String {
    // Tags that should produce a newline when encountered
    const BLOCK_TAGS: &[&str] = &[
        "<br", "<BR",
        "<p", "<P", "</p", "</P",
        "<div", "<DIV", "</div", "</DIV",
        "<tr", "<TR", "</tr", "</TR",
        "<td", "<TD", "</td", "</TD",
        "<li", "<LI", "</li", "</LI",
        "<h1", "<H1", "<h2", "<H2", "<h3", "<H3",
        "<h4", "<H4", "<h5", "<H5", "<h6", "<H6",
    ];
    let mut result = html.to_string();
    for tag in BLOCK_TAGS {
        result = result.replace(tag, &format!("\n{}", tag));
    }
    result
}

/// Decode the most common HTML entities.
fn decode_html_entities(text: &str) -> String {
    text
        .replace("&nbsp;",  " ")
        .replace("&#160;",  " ")
        .replace("&amp;",   "&")
        .replace("&#38;",   "&")
        .replace("&lt;",    "<")
        .replace("&#60;",   "<")
        .replace("&gt;",    ">")
        .replace("&#62;",   ">")
        .replace("&quot;",  "\"")
        .replace("&#34;",   "\"")
        .replace("&#39;",   "'")
        .replace("&apos;",  "'")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;","…")
        .replace("&bull;",  "•")
        .replace("&#8203;", "") // zero-width space
        .replace("&#xA0;",  " ")
}
