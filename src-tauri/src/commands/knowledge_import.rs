#![allow(dead_code)]
use crate::{api_client::{daemon_delete, daemon_get, daemon_patch, daemon_post, daemon_put}, error::AppError, state::AppState};
use chrono::Datelike as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, State};
use url::Url;
use uuid::Uuid;

// ── Data Structures ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportSession {
    pub session_id: String,
    pub seed_url: String,
    pub site_name: String,
    pub root_folder: String,
    pub status: String,
    pub created_at: i64, // ms timestamp
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportSessionSummary {
    pub session_id: String,
    pub seed_url: String,
    pub site_name: String,
    pub root_folder: String,
    pub status: String,
    pub created_at: i64,
    pub total_pages: i64,
    pub imported_pages: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportPage {
    pub page_id: String,
    pub session_id: String,
    pub url: String,
    pub title: String,
    pub parent_url: Option<String>,
    pub depth: i64,
    pub note_path: Option<String>,
    pub content_hash: Option<String>,
    pub status: String,
    pub last_crawled: Option<i64>, // ms timestamp
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportPageResult {
    pub note_path: String,
    pub title: String,
    pub was_updated: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PageUpdateInfo {
    pub page_id: String,
    pub url: String,
    pub title: String,
    pub note_path: String,
    pub new_content: String, // new markdown content
}

// ── Internal DB row types ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SessionRow {
    session_id: String,
    seed_url: String,
    site_name: String,
    root_folder: String,
    status: String,
    created_at: i64, // ms timestamp
}

#[derive(Deserialize, Clone)]
struct PageRow {
    page_id: String,
    session_id: String,
    url: String,
    title: String,
    parent_url: Option<String>,
    depth: i64,
    note_path: Option<String>,
    content_hash: Option<String>,
    http_etag: Option<String>,
    status: String,
    last_crawled: Option<i64>, // ms timestamp
}

impl From<PageRow> for ImportPage {
    fn from(r: PageRow) -> Self {
        ImportPage {
            page_id: r.page_id,
            session_id: r.session_id,
            url: r.url,
            title: r.title,
            parent_url: r.parent_url,
            depth: r.depth,
            note_path: r.note_path,
            content_hash: r.content_hash,
            status: r.status,
            last_crawled: r.last_crawled,
        }
    }
}

/// Imported page item used for RAG context building.
struct PageItem {
    url: String,
    title: String,
    content: String,
}

// ── URL validation (mirrors import.rs) ──────────────────────────────────────

fn validate_url(raw_url: &str) -> Result<Url, AppError> {
    let parsed = Url::parse(raw_url)?;

    if !["http", "https"].contains(&parsed.scheme()) {
        return Err(AppError::Security(format!(
            "不支援的協定：{}，只允許 http/https",
            parsed.scheme()
        )));
    }

    if let Some(host) = parsed.host_str() {
        let blocked = [
            "localhost", "127.", "0.0.0.0", "::1",
            "192.168.", "10.", "172.16.", "172.17.", "172.18.",
            "172.19.", "172.20.", "172.21.", "172.22.", "172.23.",
            "172.24.", "172.25.", "172.26.", "172.27.", "172.28.",
            "172.29.", "172.30.", "172.31.", "169.254.",
        ];
        for pattern in &blocked {
            if host.starts_with(pattern) || host == *pattern {
                return Err(AppError::Security(format!(
                    "不允許存取內網地址：{}", host
                )));
            }
        }
    }

    Ok(parsed)
}

// ── HTML helpers ─────────────────────────────────────────────────────────────

fn extract_title(html: &str) -> String {
    let lower = html.to_lowercase();
    if let Some(start) = lower.find("<title>") {
        if let Some(end) = lower[start..].find("</title>") {
            let raw = &html[start + 7..start + end];
            let cleaned = raw.trim().to_string();
            if !cleaned.is_empty() {
                return decode_entities(&cleaned);
            }
        }
    }
    String::new()
}

/// Extract all same-domain links from HTML.
/// Returns Vec<(absolute_url, link_text)>.
fn extract_links(html: &str, base_url: &Url) -> Vec<(String, String)> {
    let base_domain = base_url.host_str().unwrap_or("").to_lowercase();
    let mut results = Vec::new();
    let lower = html.to_lowercase();
    let mut search_start = 0;

    while let Some(tag_start) = lower[search_start..].find("<a ") {
        let abs_start = search_start + tag_start;
        // find end of opening <a ...>
        let tag_end = match lower[abs_start..].find('>') {
            Some(e) => abs_start + e + 1,
            None => break,
        };
        let tag_content = &html[abs_start..tag_end];
        let tag_lower = tag_content.to_lowercase();

        // extract href value
        if let Some(href_pos) = tag_lower.find("href=") {
            let after_href = &tag_content[href_pos + 5..];
            let href = if after_href.starts_with('"') {
                // "..."
                after_href[1..].split('"').next().unwrap_or("").trim()
            } else if after_href.starts_with('\'') {
                after_href[1..].split('\'').next().unwrap_or("").trim()
            } else {
                after_href.split_whitespace().next().unwrap_or("").trim_end_matches('>')
            };

            // skip anchors, mailto, javascript
            if !href.is_empty()
                && !href.starts_with('#')
                && !href.starts_with("mailto:")
                && !href.starts_with("javascript:")
            {
                // resolve to absolute URL
                let resolved = if href.starts_with("http://") || href.starts_with("https://") {
                    href.to_string()
                } else {
                    match base_url.join(href) {
                        Ok(u) => u.to_string(),
                        Err(_) => String::new(),
                    }
                };

                if !resolved.is_empty() {
                    // check same domain
                    if let Ok(parsed) = Url::parse(&resolved) {
                        let link_domain = parsed.host_str().unwrap_or("").to_lowercase();
                        if link_domain == base_domain {
                            // strip fragment and query for dedup key
                            let mut clean = parsed.clone();
                            clean.set_fragment(None);
                            clean.set_query(None);
                            let clean_str = clean.to_string();

                            // extract link text
                            let text_start = tag_end;
                            let link_text = if let Some(close_pos) = html[text_start..].to_lowercase().find("</a>") {
                                let raw = &html[text_start..text_start + close_pos];
                                strip_tags(raw).trim().to_string()
                            } else {
                                String::new()
                            };

                            results.push((clean_str, link_text));
                        }
                    }
                }
            }
        }

        search_start = tag_end;
    }

    results
}

/// Remove block-level tags entirely (including content between them).
fn remove_block(html: &str, tag: &str) -> String {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut result = String::new();
    let mut remaining = html;
    loop {
        let lower = remaining.to_lowercase();
        match lower.find(&open) {
            Some(start) => {
                result.push_str(&remaining[..start]);
                match lower[start..].find(&close) {
                    Some(end) => {
                        remaining = &remaining[start + end + close.len()..];
                    }
                    None => break,
                }
            }
            None => {
                result.push_str(remaining);
                break;
            }
        }
    }
    result
}

fn strip_tags(html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&#39;", "'")
        .replace("&#34;", "\"")
}

/// Rich HTML → Markdown conversion.
fn html_to_markdown_rich(html: &str) -> String {
    // Remove noise blocks entirely
    let html = remove_block(html, "script");
    let html = remove_block(&html, "style");
    let html = remove_block(&html, "nav");
    let html = remove_block(&html, "header");
    let html = remove_block(&html, "footer");

    // Process line by line is insufficient for block elements; do a sequential pass
    let mut out = String::new();
    let mut chars = html.chars().peekable();
    let mut buf = String::new(); // text accumulator inside a tag

    // We'll do a simple state machine over the raw HTML
    // collecting tag names and converting them
    let lower = html.to_lowercase();
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == b'<' {
            // find end of tag
            let tag_end = lower[i..].find('>').map(|e| i + e + 1).unwrap_or(len);
            let tag_inner = &html[i + 1..tag_end - 1]; // content between < and >
            let tag_inner_lower = tag_inner.trim().to_lowercase();
            let is_closing = tag_inner_lower.starts_with('/');
            let tag_name = tag_inner_lower
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();

            match tag_name.as_str() {
                "h1" if !is_closing => out.push_str("\n\n# "),
                "h2" if !is_closing => out.push_str("\n\n## "),
                "h3" if !is_closing => out.push_str("\n\n### "),
                "h4" if !is_closing => out.push_str("\n\n#### "),
                "h5" if !is_closing => out.push_str("\n\n##### "),
                "h6" if !is_closing => out.push_str("\n\n###### "),
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if is_closing => out.push('\n'),
                "p" if !is_closing => out.push_str("\n\n"),
                "p" if is_closing => out.push('\n'),
                "br" => out.push('\n'),
                "strong" | "b" if !is_closing => out.push_str("**"),
                "strong" | "b" if is_closing => out.push_str("**"),
                "em" | "i" if !is_closing => out.push('*'),
                "em" | "i" if is_closing => out.push('*'),
                "code" if !is_closing => {
                    // check if parent is pre (handled below); just emit backtick here
                    out.push('`');
                }
                "code" if is_closing => out.push('`'),
                "pre" if !is_closing => out.push_str("\n\n```\n"),
                "pre" if is_closing => out.push_str("\n```\n\n"),
                "li" if !is_closing => out.push_str("\n- "),
                "li" if is_closing => {}
                "ul" | "ol" if !is_closing => out.push('\n'),
                "ul" | "ol" if is_closing => out.push('\n'),
                "a" if !is_closing => {
                    // extract href from tag_inner
                    let ti_lower = tag_inner.to_lowercase();
                    let href = if let Some(hp) = ti_lower.find("href=") {
                        let after = &tag_inner[hp + 5..];
                        if after.starts_with('"') {
                            after[1..].split('"').next().unwrap_or("").to_string()
                        } else if after.starts_with('\'') {
                            after[1..].split('\'').next().unwrap_or("").to_string()
                        } else {
                            after.split_whitespace().next().unwrap_or("").trim_end_matches('>').to_string()
                        }
                    } else {
                        String::new()
                    };
                    out.push('[');
                    buf = href; // reuse buf to store href temporarily
                }
                "a" if is_closing => {
                    out.push_str("](");
                    out.push_str(&buf);
                    out.push(')');
                    buf.clear();
                }
                _ => {} // ignore other tags
            }

            i = tag_end;
        } else {
            // plain text character
            let ch = chars.next().unwrap_or(' ');
            let _ = ch; // chars iterator may be out of sync; use byte-based approach
            // Collect until next '<'
            let text_end = lower[i..].find('<').map(|e| i + e).unwrap_or(len);
            let text = &html[i..text_end];
            out.push_str(&decode_entities(text));
            i = text_end;
        }
        // sync chars iterator
        chars = html[i..].chars().peekable();
    }

    // Collapse excessive blank lines
    let mut final_lines: Vec<&str> = Vec::new();
    let mut blank_count = 0u32;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                final_lines.push("");
            }
        } else {
            blank_count = 0;
            final_lines.push(trimmed);
        }
    }

    final_lines.join("\n").trim().to_string()
}

/// Slugify a title: lowercase, spaces → dashes, remove non-alphanumeric except dash.
fn slugify(title: &str) -> String {
    let lower = title.to_lowercase();
    let mut slug = String::new();
    for ch in lower.chars() {
        if ch.is_alphanumeric() {
            slug.push(ch);
        } else if ch == ' ' || ch == '-' || ch == '_' {
            if !slug.ends_with('-') {
                slug.push('-');
            }
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "page".to_string()
    } else {
        slug
    }
}

fn sha256_hex(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

// ── Tauri Commands ────────────────────────────────────────────────────────────

/// Create a new import session for the given seed URL.
#[tauri::command]
pub async fn create_import_session(
    state: State<'_, AppState>,
    seed_url: String,
) -> Result<ImportSession, AppError> {
    let parsed = validate_url(&seed_url)?;
    let site_name = parsed.host_str().unwrap_or("unknown").to_string();
    let sanitized_domain = site_name.replace('.', "-");
    let root_folder = format!("imports/{}", sanitized_domain);
    let created_at_ms = chrono::Utc::now().timestamp_millis();

    let vault_id = state.get_vault_uuid().await;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    // Persist session in daemon; daemon returns {"session_id": "..."}
    if !vault_id.is_empty() {
        let path = format!("/vaults/{}/kb/sessions", urlencoding::encode(&vault_id));
        let resp = daemon_post::<_, serde_json::Value>(
            &state.http_client,
            &path,
            &serde_json::json!({
                "seed_url": seed_url,
                "site_name": site_name,
                "root_folder": root_folder,
            }),
            tok,
        ).await.map_err(|e| AppError::Import(format!("建立 session 失敗：{}", e)))?;
        // Daemon returns {"session_id": "..."}; use its session_id
        let session_id = resp["session_id"].as_str()
            .ok_or_else(|| AppError::Import("daemon 未回傳 session_id".to_string()))?
            .to_string();
        return Ok(ImportSession {
            session_id,
            seed_url,
            site_name,
            root_folder,
            status: "pending".to_string(),
            created_at: created_at_ms,
        });
    }

    // Fallback (no vault): in-memory only
    let session_id = Uuid::new_v4().to_string();
    Ok(ImportSession {
        session_id,
        seed_url,
        site_name,
        root_folder,
        status: "pending".to_string(),
        created_at: created_at_ms,
    })
}

/// List all import sessions for the current vault.
#[tauri::command]
pub async fn list_import_sessions(
    state: State<'_, AppState>,
) -> Result<Vec<ImportSessionSummary>, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(vec![]); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!("/vaults/{}/kb/sessions", urlencoding::encode(&vault_id));
    let result: serde_json::Value = daemon_get(&state.http_client, &path, tok)
        .await
        .unwrap_or(serde_json::json!([]));
    let arr = result.as_array().cloned().unwrap_or_default();
    let sessions = arr.iter().filter_map(|v| {
        let session_id = v["session_id"].as_str()?.to_string();
        Some(ImportSessionSummary {
            session_id,
            seed_url: v["seed_url"].as_str().unwrap_or("").to_string(),
            site_name: v["site_name"].as_str().unwrap_or("").to_string(),
            root_folder: v["root_folder"].as_str().unwrap_or("").to_string(),
            status: v["status"].as_str().unwrap_or("active").to_string(),
            created_at: v["created_at"].as_i64().unwrap_or(0),
            total_pages: v["total_pages"].as_i64().unwrap_or(0),
            imported_pages: v["imported_pages"].as_i64().unwrap_or(0),
        })
    }).collect();
    Ok(sessions)
}

/// Delete an import session and all its pages (including chunks and KB suggestions).
#[tauri::command]
pub async fn delete_import_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(()); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!(
        "/vaults/{}/kb/sessions/{}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&session_id),
    );
    let _ = daemon_delete::<serde_json::Value>(&state.http_client, &path, tok).await;
    Ok(())
}

/// Toggle auto_update for an import session.
#[tauri::command]
pub async fn set_session_auto_update(
    state: State<'_, AppState>,
    session_id: String,
    auto_update: bool,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(()); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!(
        "/vaults/{}/kb/sessions/{}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&session_id),
    );
    let _ = daemon_patch::<_, serde_json::Value>(
        &state.http_client,
        &path,
        &serde_json::json!({ "auto_update": auto_update }),
        tok,
    ).await;
    Ok(())
}

/// Called on app startup: check_page_updates for sessions with auto_update = true.
pub async fn auto_check_all_sessions(app: &AppHandle, state: &AppState) {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return; }
    let token = state.get_auth_token().await;
    let tok_owned = token.clone();
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    let path = format!("/vaults/{}/kb/sessions", urlencoding::encode(&vault_id));
    let Ok(result) = daemon_get::<serde_json::Value>(&state.http_client, &path, tok).await else { return; };
    let sessions = result.as_array().cloned().unwrap_or_default();
    for s in sessions {
        if s["auto_update"].as_bool() != Some(true) { continue; }
        let sid = match s["session_id"].as_str() { Some(id) => id.to_string(), None => continue };
        // Spawn a fire-and-forget task per session
        let app_clone = app.clone();
        let vault_clone = vault_id.clone();
        let token_clone = token.clone();
        let http = state.http_client.clone();
        tokio::spawn(async move {
            let tok2: Option<&str> = if token_clone.is_empty() { None } else { Some(token_clone.as_str()) };
            let pages_path = format!(
                "/vaults/{}/kb/sessions/{}/pages",
                urlencoding::encode(&vault_clone),
                urlencoding::encode(&sid),
            );
            let Ok(pages_val) = daemon_get::<serde_json::Value>(&http, &pages_path, tok2).await else { return; };
            let pages = pages_val.as_array().cloned().unwrap_or_default();
            for p in pages {
                if p["status"].as_str() != Some("imported") { continue; }
                let url: String = match p["url"].as_str() { Some(u) => u.to_string(), None => continue };
                let pid: String = match p["page_id"].as_str() { Some(id) => id.to_string(), None => continue };
                let stored_hash: Option<String> = p["content_hash"].as_str().map(|s: &str| s.to_string());
                let stored_etag: Option<String> = p["http_etag"].as_str().map(|s: &str| s.to_string());
                // HEAD request to check ETag / content changes
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(15))
                    .build()
                    .unwrap_or_default();
                let Ok(head_resp) = client.head(&url).send().await else { continue };
                let new_etag = head_resp.headers()
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let changed = if let (Some(se), Some(ne)) = (&stored_etag, &new_etag) {
                    se != ne
                } else {
                    stored_hash.is_none() // no hash means never checked
                };
                if changed {
                    let _ = app_clone.emit("kb:page_update_available", serde_json::json!({
                        "session_id": sid,
                        "page_id": pid,
                        "url": url,
                    }));
                }
            }
        });
    }
}

/// Crawl the seed URL and discover all same-domain links.
/// Creates ImportPage records for all discovered pages.
#[tauri::command]
pub async fn fetch_site_outline(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<ImportPage>, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault".to_string()));
    }
    let token = state.get_auth_token().await;
    let tok_owned = token.clone();
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };

    // Get session info from daemon
    let session_path = format!(
        "/vaults/{}/kb/sessions/{}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&session_id),
    );
    let session_val: serde_json::Value = daemon_get(&state.http_client, &session_path, tok)
        .await
        .map_err(|e| AppError::Import(format!("無法取得 session: {}", e)))?;
    let seed_url = session_val["seed_url"].as_str()
        .ok_or_else(|| AppError::Import("session 缺少 seed_url".to_string()))?
        .to_string();

    let base_url = validate_url(&seed_url)?;

    // Fetch seed page HTML
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("noteTreeLM/1.0")
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;
    let html = client.get(&seed_url).send().await
        .map_err(|e| AppError::Import(format!("無法取得首頁：{}", e)))?
        .text().await
        .map_err(|e| AppError::Import(format!("讀取首頁內容失敗：{}", e)))?;

    let seed_title = extract_title(&html);
    let links = extract_links(&html, &base_url);

    // Build page list: seed page + discovered links (dedup)
    let mut seen = std::collections::HashSet::new();
    seen.insert(seed_url.clone());
    let mut pages: Vec<serde_json::Value> = vec![serde_json::json!({
        "page_id": Uuid::new_v4().to_string(),
        "session_id": session_id,
        "url": seed_url,
        "title": if seed_title.is_empty() { base_url.host_str().unwrap_or("首頁").to_string() } else { seed_title },
        "parent_url": null,
        "depth": 0,
        "status": "pending",
    })];
    for (url, text) in links {
        if seen.contains(&url) { continue; }
        seen.insert(url.clone());
        pages.push(serde_json::json!({
            "page_id": Uuid::new_v4().to_string(),
            "session_id": session_id,
            "url": url,
            "title": text,
            "parent_url": seed_url,
            "depth": 1,
            "status": "pending",
        }));
    }

    // Store pages in daemon
    let pages_path = format!(
        "/vaults/{}/kb/sessions/{}/pages",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&session_id),
    );
    let _ = daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &pages_path,
        &serde_json::json!({ "pages": pages }),
        tok,
    ).await;

    // Return as ImportPage
    let result = pages.iter().filter_map(|v| {
        let page_id = v["page_id"].as_str()?.to_string();
        Some(ImportPage {
            page_id,
            session_id: session_id.clone(),
            url: v["url"].as_str().unwrap_or("").to_string(),
            title: v["title"].as_str().unwrap_or("").to_string(),
            parent_url: v["parent_url"].as_str().map(|s| s.to_string()),
            depth: v["depth"].as_i64().unwrap_or(0),
            note_path: None,
            content_hash: None,
            status: "pending".to_string(),
            last_crawled: None,
        })
    }).collect();
    Ok(result)
}

/// Lightweight on-demand fetch for Q&A: HTTP fetch → markdown → store content_md via daemon.
async fn fetch_page_content_for_qa(
    page: &PageRow,
    vault_id: &str,
    http_client: &reqwest::Client,
) -> Result<(), AppError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("noteTreeLM/1.0")
        .build()
        .unwrap_or_default();
    let html = client.get(&page.url).send().await
        .map_err(|e| AppError::Import(format!("fetch {} failed: {}", page.url, e)))?
        .text().await
        .map_err(|e| AppError::Import(e.to_string()))?;
    let content_md = html_to_markdown_rich(&html);
    let hash = sha256_hex(&content_md);
    let now_ms = chrono::Utc::now().timestamp_millis();
    // Update page in daemon with fetched content
    let page_update_path = format!(
        "/vaults/{}/kb/sessions/{}/pages/{}",
        urlencoding::encode(vault_id),
        urlencoding::encode(&page.session_id),
        urlencoding::encode(&page.page_id),
    );
    let _ = daemon_patch::<_, serde_json::Value>(
        http_client,
        &page_update_path,
        &serde_json::json!({
            "status": "imported",
            "content_md": content_md,
            "content_hash": hash,
            "last_crawled": now_ms,
        }),
        None,
    ).await;
    Ok(())
}

/// Core import logic — kept for API compatibility but superseded by `import_page`.
#[allow(dead_code)]
async fn import_page_inner(
    page: &PageRow,
    _root_folder: &str,
) -> Result<ImportPageResult, AppError> {
    // Minimal implementation — full logic lives in `import_page` (which has AppState access).
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("noteTreeLM/1.0")
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;
    let resp = client.get(&page.url).send().await
        .map_err(|e| AppError::Import(format!("無法取得頁面：{}", e)))?;
    let html = resp.text().await
        .map_err(|e| AppError::Import(format!("讀取頁面內容失敗：{}", e)))?;
    let title = { let t = extract_title(&html); if t.is_empty() { page.title.clone() } else { t } };
    let content_md = html_to_markdown_rich(&html);
    let hash = sha256_hex(&content_md);
    let was_updated = page.content_hash.as_deref() != Some(&hash);
    Ok(ImportPageResult { note_path: page.note_path.clone().unwrap_or_default(), title, was_updated })
}

/// Import a single page — fetch HTML, write vault file, update daemon page record.
#[tauri::command]
pub async fn import_page(
    state: State<'_, AppState>,
    session_id: String,
    page_id: String,
) -> Result<ImportPageResult, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Err(AppError::Vault("尚未設定 Vault".to_string())); }
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() { return Err(AppError::Vault("尚未設定 Vault 路徑".to_string())); }
    let token = state.get_auth_token().await;
    let tok_owned = token.clone();
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };

    // Get session info for root_folder
    let session_path = format!(
        "/vaults/{}/kb/sessions/{}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&session_id),
    );
    let session_val: serde_json::Value = daemon_get(&state.http_client, &session_path, tok).await
        .map_err(|e| AppError::Import(format!("無法取得 session: {}", e)))?;
    let root_folder = session_val["root_folder"].as_str().unwrap_or("imports").to_string();

    // Get page info
    let pages_path = format!(
        "/vaults/{}/kb/sessions/{}/pages",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&session_id),
    );
    let pages_val: serde_json::Value = daemon_get(&state.http_client, &pages_path, tok).await
        .unwrap_or(serde_json::json!([]));
    let page_v = pages_val.as_array()
        .and_then(|arr| arr.iter().find(|v| v["page_id"].as_str() == Some(&page_id)))
        .cloned()
        .ok_or_else(|| AppError::Import(format!("page not found: {}", page_id)))?;

    let page = PageRow {
        page_id: page_id.clone(),
        session_id: session_id.clone(),
        url: page_v["url"].as_str().unwrap_or("").to_string(),
        title: page_v["title"].as_str().unwrap_or("").to_string(),
        parent_url: page_v["parent_url"].as_str().map(|s| s.to_string()),
        depth: page_v["depth"].as_i64().unwrap_or(0),
        note_path: page_v["note_path"].as_str().map(|s| s.to_string()),
        content_hash: page_v["content_hash"].as_str().map(|s| s.to_string()),
        http_etag: page_v["http_etag"].as_str().map(|s| s.to_string()),
        status: page_v["status"].as_str().unwrap_or("pending").to_string(),
        last_crawled: page_v["last_crawled"].as_i64(),
    };

    // Fetch HTML
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("noteTreeLM/1.0")
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;
    let resp = client.get(&page.url).send().await
        .map_err(|e| AppError::Import(format!("無法取得頁面：{}", e)))?;
    let new_etag = resp.headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let html = resp.text().await
        .map_err(|e| AppError::Import(format!("讀取頁面內容失敗：{}", e)))?;

    let title = { let t = extract_title(&html); if t.is_empty() { page.title.clone() } else { t } };
    let content_md = html_to_markdown_rich(&html);
    let hash = sha256_hex(&content_md);
    let was_updated = page.content_hash.as_deref() != Some(&hash);

    // Determine note path
    let note_rel_path = if let Some(ref np) = page.note_path {
        np.clone()
    } else {
        let slug = slugify(&title);
        format!("{}/{}.md", root_folder, slug)
    };

    // Build frontmatter + content
    let frontmatter = format!(
        "---\ntitle: {}\nsource: {}\nimported_at: {}\n---\n\n",
        title.replace(':', "："),
        page.url,
        chrono::Utc::now().format("%Y-%m-%d"),
    );
    let file_content = format!("{}{}", frontmatter, content_md);

    // Write vault file
    let abs_path = std::path::PathBuf::from(&vault_path).join(&note_rel_path);
    if let Some(parent) = abs_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&abs_path, file_content.as_bytes()).await?;

    // Sync vault note to daemon FTS index (no file watcher in daemon)
    let _ = daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
        &serde_json::json!({"path": note_rel_path, "content": file_content}),
        tok,
    ).await;

    // Update page in daemon
    let page_update_path = format!(
        "/vaults/{}/kb/sessions/{}/pages/{}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&session_id),
        urlencoding::encode(&page_id),
    );
    let now_ms = chrono::Utc::now().timestamp_millis();
    let _ = daemon_patch::<_, serde_json::Value>(
        &state.http_client,
        &page_update_path,
        &serde_json::json!({
            "status": "imported",
            "note_path": note_rel_path,
            "content_md": content_md,
            "content_hash": hash,
            "http_etag": new_etag,
            "last_crawled": now_ms,
            "title": title,
        }),
        tok,
    ).await;

    Ok(ImportPageResult { note_path: note_rel_path, title, was_updated })
}

/// Check which already-imported pages have updated content (via ETag or content hash).
#[tauri::command]
pub async fn check_page_updates(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<PageUpdateInfo>, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(vec![]); }
    let token = state.get_auth_token().await;
    let tok_owned = token.clone();
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };

    let pages_path = format!(
        "/vaults/{}/kb/sessions/{}/pages",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&session_id),
    );
    let result: serde_json::Value = daemon_get(&state.http_client, &pages_path, tok).await
        .unwrap_or(serde_json::json!([]));
    let pages = result.as_array().cloned().unwrap_or_default();

    let mut updates = Vec::new();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("noteTreeLM/1.0")
        .build()
        .unwrap_or_default();

    for p in pages {
        if p["status"].as_str() != Some("imported") { continue; }
        let url = match p["url"].as_str() { Some(u) => u.to_string(), None => continue };
        let page_id = match p["page_id"].as_str() { Some(id) => id.to_string(), None => continue };
        let title = p["title"].as_str().unwrap_or("").to_string();
        let note_path = p["note_path"].as_str().unwrap_or("").to_string();
        let stored_hash = p["content_hash"].as_str().map(|s| s.to_string());
        let stored_etag = p["http_etag"].as_str().map(|s| s.to_string());

        // Try ETag first via HEAD
        let head_result = client.head(&url).send().await;
        if let Ok(head_resp) = head_result {
            if let Some(new_etag) = head_resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(|s| s.to_string()) {
                if Some(&new_etag) != stored_etag.as_ref() {
                    // Fetch full page to get new content
                    if let Ok(get_resp) = client.get(&url).send().await {
                        if let Ok(html) = get_resp.text().await {
                            let new_content = html_to_markdown_rich(&html);
                            updates.push(PageUpdateInfo { page_id, url, title, note_path, new_content });
                        }
                    }
                }
                continue;
            }
        }
        // Fallback: fetch and compare hash
        let Ok(get_resp) = client.get(&url).send().await else { continue };
        let Ok(html) = get_resp.text().await else { continue };
        let new_content = html_to_markdown_rich(&html);
        let new_hash = sha256_hex(&new_content);
        if Some(new_hash) != stored_hash {
            updates.push(PageUpdateInfo { page_id, url, title, note_path, new_content });
        }
    }

    Ok(updates)
}

/// Get all pages for a given session, sorted by depth then title.
#[tauri::command]
pub async fn get_session_pages(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<ImportPage>, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(vec![]); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!(
        "/vaults/{}/kb/sessions/{}/pages",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&session_id),
    );
    let result: serde_json::Value = daemon_get(&state.http_client, &path, tok)
        .await
        .unwrap_or(serde_json::json!([]));
    let arr = result.as_array().cloned().unwrap_or_default();
    let mut pages: Vec<ImportPage> = arr.iter().filter_map(|v| {
        let page_id = v["page_id"].as_str()?.to_string();
        Some(ImportPage {
            page_id,
            session_id: session_id.clone(),
            url: v["url"].as_str().unwrap_or("").to_string(),
            title: v["title"].as_str().unwrap_or("").to_string(),
            parent_url: v["parent_url"].as_str().map(|s| s.to_string()),
            depth: v["depth"].as_i64().unwrap_or(0),
            note_path: v["note_path"].as_str().map(|s| s.to_string()),
            content_hash: v["content_hash"].as_str().map(|s| s.to_string()),
            status: v["status"].as_str().unwrap_or("pending").to_string(),
            last_crawled: v["last_crawled"].as_i64(),
        })
    }).collect();
    pages.sort_by(|a, b| a.depth.cmp(&b.depth).then(a.title.cmp(&b.title)));
    Ok(pages)
}

// ── Knowledge Q&A ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KnowledgeRef {
    pub path: String,
    pub title: String,
    pub excerpt: String,
}

/// Start a streaming RAG Q&A query over imported notes.
/// query_id is generated by the frontend and passed in; events emitted: knowledge:token, knowledge:refs, knowledge:done.
#[tauri::command]
pub async fn query_knowledge(
    app: AppHandle,
    state: State<'_, AppState>,
    query_id: String,
    question: String,
    session_id: Option<String>,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() {
        let _ = app.emit("knowledge:done", serde_json::json!({ "query_id": &query_id }));
        return Ok(());
    }
    let app_clone = app.clone();
    let state_ref = state.inner();
    run_knowledge_query(&app_clone, &vault_id, &question, session_id.as_deref(), &query_id, state_ref).await
}

/// Search already-imported pages relevant to `question` via daemon FTS on import_pages.
async fn find_relevant_imported_pages(
    vault_id: &str,
    session_id: Option<&str>,
    question: &str,
) -> Vec<PageItem> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let url = if let Some(sid) = session_id {
        format!(
            "http://127.0.0.1:7787/api/v1/vaults/{}/kb/sessions/{}/pages?status=imported&q={}",
            urlencoding::encode(vault_id),
            urlencoding::encode(sid),
            urlencoding::encode(question),
        )
    } else {
        format!(
            "http://127.0.0.1:7787/api/v1/vaults/{}/kb/pages?status=imported&q={}",
            urlencoding::encode(vault_id),
            urlencoding::encode(question),
        )
    };
    let Ok(resp) = client.get(&url).send().await else { return vec![]; };
    let Ok(json) = resp.json::<serde_json::Value>().await else { return vec![]; };
    let arr = json.as_array().cloned().unwrap_or_default();
    arr.iter().filter_map(|v| {
        let content_md = v["content_md"].as_str()?;
        if content_md.is_empty() { return None; }
        Some(PageItem {
            url: v["url"].as_str().unwrap_or("").to_string(),
            title: v["title"].as_str().unwrap_or("").to_string(),
            content: content_md.to_string(),
        })
    }).collect()
}

/// Find pending pages matching a question (title FTS on import_pages).
async fn find_matching_pending_pages(
    vault_id: &str,
    session_id: Option<&str>,
    question: &str,
    _emb_url: Option<&str>,
) -> Vec<PageRow> {
    // Search pending pages in daemon by title keyword match
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let base = if let Some(sid) = session_id {
        format!(
            "http://127.0.0.1:7787/api/v1/vaults/{}/kb/sessions/{}/pages?status=pending&q={}",
            urlencoding::encode(vault_id),
            urlencoding::encode(sid),
            urlencoding::encode(question),
        )
    } else {
        // Without session_id we can't easily query — return empty
        return vec![];
    };
    let Ok(resp) = client.get(&base).send().await else { return vec![]; };
    let Ok(json) = resp.json::<serde_json::Value>().await else { return vec![]; };
    let arr = json.as_array().cloned().unwrap_or_default();
    arr.iter().filter_map(|v| {
        let page_id = v["page_id"].as_str()?.to_string();
        Some(PageRow {
            page_id,
            session_id: session_id.unwrap_or("").to_string(),
            url: v["url"].as_str().unwrap_or("").to_string(),
            title: v["title"].as_str().unwrap_or("").to_string(),
            parent_url: v["parent_url"].as_str().map(|s| s.to_string()),
            depth: v["depth"].as_i64().unwrap_or(0),
            note_path: v["note_path"].as_str().map(|s| s.to_string()),
            content_hash: v["content_hash"].as_str().map(|s| s.to_string()),
            http_etag: v["http_etag"].as_str().map(|s| s.to_string()),
            status: v["status"].as_str().unwrap_or("pending").to_string(),
            last_crawled: v["last_crawled"].as_i64(),
        })
    }).collect()
}

async fn run_knowledge_query(
    _app: &AppHandle,
    vault_id: &str,
    question: &str,
    session_id: Option<&str>,
    query_id: &str,
    app_state: &AppState,
) -> Result<(), AppError> {
    // All logic (FTS, on-demand fetch, LLM streaming, cite enforcement, agent:refs)
    // is handled by the kb_query agent in the service via /vaults/:vid/agent/run.
    // source_type/source_id are forwarded so search_kb_pages can scope by session.
    let mut body = serde_json::json!({
        "agent":           "kb_query",
        "session_id":      query_id,
        "input":           question,
        "conversation_id": query_id,
    });
    if let Some(sid) = session_id {
        body["source_type"] = serde_json::json!("kb_session");
        body["source_id"]   = serde_json::json!(sid);
    };
    let token = app_state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    daemon_post::<_, serde_json::Value>(
        &app_state.http_client,
        &format!("/vaults/{}/agent/run", vault_id),
        &body,
        tok,
    )
    .await
    .map_err(|e| AppError::AI(format!("KB query 失敗：{}", e)))?;

    Ok(())
}

// ── KB Card Suggestion ────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct KBCardSuggestion {
    pub title: String,
    pub template: String,   // "concept" | "procedure" | "reference"
    pub content: String,    // 預填的 markdown 內容（含 frontmatter）
    pub reason: String,     // 為什麼建議這張卡片
}

/// LLM 建議的技能規範（尚未持久化）
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentSkillSuggestion {
    pub title: String,
    pub trigger: String,          // "當問題涉及 X、Y、Z 時"
    pub behavior: String,         // agent 應執行的操作指令
    #[serde(default)]
    pub tool_calls: Vec<String>, // 自動呼叫的工具，限 search_vault/read_note/list_structure
    #[serde(default = "default_passive")]
    pub injection_mode: String,   // "passive"（embedding 比對）或 "active"（永遠注入）
    #[serde(default = "default_scope_all")]
    pub agent_scope: String,      // "all" | "main" | "search" | "write" | "research" | "memory"
    #[serde(default)]
    pub need_tool_chain: bool,    // 需要嚴格依序執行工具時為 true
    #[serde(default)]
    pub tool_chain_order: Vec<String>, // 工具執行順序（need_tool_chain=true 時有效）
}

fn default_passive() -> String { "passive".to_string() }
fn default_scope_all() -> String { "all".to_string() }

fn valid_scope(s: &str) -> &str {
    match s {
        "main" | "search" | "write" | "research" | "memory" => s,
        _ => "all",
    }
}

/// 持久化後的技能規範（從 DB 讀取）
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentSkillRecord {
    pub skill_id: String,
    pub vault_id: String,
    pub knowledge_item_id: String,
    pub title: String,
    pub trigger: String,
    pub behavior: String,
    pub tool_calls: Vec<String>,
    pub is_active: bool,
    pub injection_mode: String,   // "passive" | "active"
    pub agent_scope: String,      // "all" | "main" | "search" | "write" | "research" | "memory"
    pub need_tool_chain: bool,
    pub tool_chain_order: Vec<String>,
    pub trigger_count: i64,
    pub last_triggered_at: Option<i64>, // ms timestamp，None = 從未觸發
    pub created_at: i64,
}

/// suggest_kb_cards_for_item 的回傳格式（筆記卡片 + 技能規範）
#[derive(Debug, Serialize, Deserialize)]
pub struct KBAndSkillSuggestions {
    pub note_cards: Vec<KBCardSuggestion>,
    pub skill_cards: Vec<AgentSkillSuggestion>,
}

/// 根據已匯入頁面的內容，用 LLM 建議 2-4 個值得建立的知識卡片。
#[tauri::command]
pub async fn suggest_kb_cards(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    page_id: String,
) -> Result<Vec<KBCardSuggestion>, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(vec![]); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    // Get page content from daemon
    let pages_path = format!(
        "/vaults/{}/kb/sessions/{}/pages",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&session_id),
    );
    let pages_val: serde_json::Value = daemon_get(&state.http_client, &pages_path, tok).await
        .unwrap_or(serde_json::json!([]));
    let page_v = pages_val.as_array()
        .and_then(|arr| arr.iter().find(|v| v["page_id"].as_str() == Some(&page_id)))
        .cloned()
        .ok_or_else(|| AppError::Import(format!("page not found: {}", page_id)))?;
    let content_md = page_v["content_md"].as_str().unwrap_or("");
    if content_md.is_empty() {
        return Err(AppError::Import("頁面尚未匯入內容".to_string()));
    }

    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };

    let user_content = format!(
        "頁面標題：{}\n\n內容：\n{}",
        page_v["title"].as_str().unwrap_or(""),
        &content_md.chars().take(2000).collect::<String>(),
    );

    let resp: serde_json::Value = crate::api_client::daemon_post(
        &state.http_client,
        &format!("/vaults/{}/agent/invoke", urlencoding::encode(&vault_id)),
        &serde_json::json!({
            "system":      note_card_system_prompt(),
            "input":       user_content,
            "temperature": 0.3,
            "max_tokens":  1500,
        }),
        tok,
    ).await.map_err(|e| AppError::AI(e))?;

    let raw = resp["text"].as_str().unwrap_or("").trim().to_string();
    let obj_start = raw.find('{').unwrap_or(0);
    let obj_end = raw.rfind('}').map(|i| i + 1).unwrap_or(raw.len());

    #[derive(Deserialize, Default)]
    struct NoteCardsOnly { #[serde(default)] note_cards: Vec<KBCardSuggestion> }
    let parsed: NoteCardsOnly = serde_json::from_str(&raw[obj_start..obj_end]).unwrap_or_default();

    // Persist suggestions to daemon
    for card in &parsed.note_cards {
        let suggestion_id = Uuid::new_v4().to_string();
        let _ = daemon_post::<_, serde_json::Value>(
            &state.http_client,
            &format!("/vaults/{}/kb/suggestions", urlencoding::encode(&vault_id)),
            &serde_json::json!({
                "suggestion_id": suggestion_id,
                "vault_id": vault_id,
                "session_id": session_id,
                "page_id": page_id,
                "title": card.title,
                "template": card.template,
                "content": card.content,
                "reason": card.reason,
                "created_at": chrono::Utc::now().timestamp(),
            }),
            tok,
        ).await;
    }

    Ok(parsed.note_cards)
}

/// 載入已存入 daemon 的知識卡片建議（按 session 或 page 過濾）
#[tauri::command]
pub async fn list_kb_suggestions(
    state: State<'_, AppState>,
    session_id: Option<String>,
    page_id: Option<String>,
) -> Result<Vec<KBSuggestionRecord>, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(vec![]); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let mut path = format!("/vaults/{}/kb/suggestions", urlencoding::encode(&vault_id));
    let mut query_parts = Vec::new();
    if let Some(ref sid) = session_id {
        query_parts.push(format!("session_id={}", urlencoding::encode(sid)));
    }
    if let Some(ref pid) = page_id {
        query_parts.push(format!("page_id={}", urlencoding::encode(pid)));
    }
    if !query_parts.is_empty() {
        path = format!("{}?{}", path, query_parts.join("&"));
    }
    let result: serde_json::Value = daemon_get(&state.http_client, &path, tok)
        .await
        .unwrap_or(serde_json::json!([]));
    let arr = result.as_array().cloned().unwrap_or_default();
    let records = arr.iter().filter_map(|v| {
        let suggestion_id = v["suggestion_id"].as_str()?.to_string();
        Some(KBSuggestionRecord {
            suggestion_id,
            session_id: v["session_id"].as_str().unwrap_or("").to_string(),
            page_id: v["page_id"].as_str().unwrap_or("").to_string(),
            title: v["title"].as_str().unwrap_or("").to_string(),
            template: v["template"].as_str().unwrap_or("concept").to_string(),
            content: v["content"].as_str().unwrap_or("").to_string(),
            reason: v["reason"].as_str().unwrap_or("").to_string(),
            created_at: v["created_at"].as_i64().unwrap_or(0),
        })
    }).collect();
    Ok(records)
}

/// 刪除單筆建議
#[tauri::command]
pub async fn dismiss_kb_suggestion(
    state: State<'_, AppState>,
    suggestion_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(()); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!(
        "/vaults/{}/kb/suggestions/{}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&suggestion_id),
    );
    let _ = daemon_delete::<serde_json::Value>(&state.http_client, &path, tok).await;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct KBSuggestionRecord {
    pub suggestion_id: String,
    pub session_id: String,
    pub page_id: String,
    pub title: String,
    pub template: String,
    pub content: String,
    pub reason: String,
    pub created_at: i64,
}

// ── Wikilink injection helper ─────────────────────────────────────────────────

/// Inject [[wikilinks]] to sibling session notes into card content.
///
/// Strategy:
/// 1. For each sibling title, if it appears verbatim in the body (outside frontmatter),
///    wrap the first occurrence with [[title]].
/// 2. Collect all sibling titles that either got wikilinked inline OR appear as keywords
///    in the existing "## 相關概念" section (if present).
/// 3. Fill/append a "## 相關筆記" section with all wikilinked siblings.
fn inject_wikilinks_to_siblings(content: &str, sibling_titles: &[(String, String)]) -> String {
    if sibling_titles.is_empty() {
        return content.to_string();
    }

    // Split frontmatter from body
    let (frontmatter, body) = if content.starts_with("---") {
        // Find the closing ---
        if let Some(end_offset) = content[3..].find("\n---") {
            let split = 3 + end_offset + 4; // after the closing ---
            // Consume the trailing newline if any
            let split = if content.as_bytes().get(split) == Some(&b'\n') { split + 1 } else { split };
            (&content[..split], &content[split..])
        } else {
            ("", content)
        }
    } else {
        ("", content)
    };

    let mut body_str = body.to_string();
    let mut linked_titles: Vec<&str> = Vec::new();

    for (title, _path) in sibling_titles {
        let wikilink = format!("[[{}]]", title);
        // Already wikilinked in body?
        if body_str.contains(&wikilink) {
            linked_titles.push(title.as_str());
            continue;
        }
        // Find verbatim title occurrence in body (case-sensitive for CJK safety)
        if let Some(pos) = body_str.find(title.as_str()) {
            // Don't double-wrap
            let before = &body_str[..pos];
            if !before.ends_with("[[") {
                let after = body_str[pos + title.len()..].to_string();
                body_str = format!("{}[[{}]]{}", before, title, after);
                linked_titles.push(title.as_str());
            }
        } else {
            // Title not found inline — still include in 相關筆記 section
            linked_titles.push(title.as_str());
        }
    }

    if linked_titles.is_empty() {
        return format!("{}{}", frontmatter, body_str);
    }

    // Fill "## 相關概念" placeholder bullets OR append "## 相關筆記"
    let related_section = linked_titles.iter()
        .map(|t| format!("- [[{}]]", t))
        .collect::<Vec<_>>()
        .join("\n");

    // Replace empty "## 相關概念\n\n-\n" placeholder generated by LLM
    if body_str.contains("## 相關概念\n\n-\n") {
        body_str = body_str.replacen(
            "## 相關概念\n\n-\n",
            &format!("## 相關概念\n\n{}\n", related_section),
            1,
        );
    } else if body_str.contains("## 相關概念\n\n- \n") {
        body_str = body_str.replacen(
            "## 相關概念\n\n- \n",
            &format!("## 相關概念\n\n{}\n", related_section),
            1,
        );
    } else if !body_str.contains("## 相關筆記") {
        // Append new section
        body_str.push_str(&format!("\n## 相關筆記\n\n{}\n", related_section));
    }

    format!("{}{}", frontmatter, body_str)
}

/// Create a vault note from a KB card suggestion, auto-injecting wikilinks
/// to other notes already created from the same session.
/// Also marks the suggestion as created (stores note_path) and dismisses it.
#[tauri::command]
pub async fn create_kb_card_note(
    state: State<'_, AppState>,
    _suggestion_id: String,
    _session_id: String,
    title: String,
    content: String,
) -> Result<String, AppError> {
    let vault_id = state.get_vault_id().await?;
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }

    // DB removed: sibling wikilinks skipped (daemon handles indexing)
    let final_content = content.clone();

    // Create vault note (mirrors create_note logic)
    let safe_title: String = title.chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    let filename = format!("{}.md", safe_title.trim());
    let rel_path = filename.clone();

    let abs_path = std::path::PathBuf::from(&vault_path).join(&rel_path);
    tokio::fs::write(&abs_path, final_content.as_bytes()).await?;

    // Sync to daemon for search indexing
    if !vault_id.is_empty() {
        let token = state.get_auth_token().await;
        let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
        let _ = daemon_post::<_, serde_json::Value>(
            &state.http_client,
            &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
            &serde_json::json!({ "path": rel_path, "content": final_content }),
            tok,
        ).await;
    }

    Ok(rel_path)
}

/// 搜尋已驗證 KB chunks，回傳格式化的 context 字串供注入 system prompt。
pub async fn search_kb_context(
    vault_id: &str,
    query: &str,
    _embedding_url: Option<&str>,
) -> Option<String> {
    // Search via daemon search endpoint
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let url = format!(
        "http://127.0.0.1:7787/api/v1/vaults/{}/search?q={}&limit=5",
        urlencoding::encode(vault_id),
        urlencoding::encode(query),
    );
    let resp = client.get(&url).send().await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    let arr = json.as_array()?;
    if arr.is_empty() { return None; }
    let context = arr.iter().enumerate().map(|(i, c)| {
        let section = c["section"].as_str().unwrap_or("");
        let source = c["path"].as_str().unwrap_or("");
        format!("[{}] {}\n來源：{}", i + 1, &section.chars().take(400).collect::<String>(), source)
    }).collect::<Vec<_>>().join("\n\n---\n\n");
    Some(format!("以下是相關知識庫內容：\n\n{}", context))
}

// ── Knowledge Items ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct KnowledgeItem {
    pub item_id: String,
    pub vault_id: String,
    pub session_id: String,
    pub title: String,
    pub source_refs: Vec<KnowledgeRef>,
    pub ai_summary: String,
    pub created_at: i64, // ms timestamp
}

/// Save an AI response + source refs as a knowledge item.
#[tauri::command]
pub async fn save_knowledge_item(
    state: State<'_, AppState>,
    session_id: String,
    title: String,
    ai_summary: String,
    source_refs: Vec<KnowledgeRef>,
) -> Result<KnowledgeItem, AppError> {
    let vault_id = state.get_vault_id().await?;
    let item_id = Uuid::new_v4().to_string();
    let created_at = chrono::Utc::now().timestamp_millis();
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!("/vaults/{}/kb/items", urlencoding::encode(&vault_id));
    let _ = daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &path,
        &serde_json::json!({
            "item_id": item_id,
            "vault_id": vault_id,
            "session_id": session_id,
            "title": title,
            "ai_summary": ai_summary,
            "source_refs": serde_json::to_string(&source_refs).unwrap_or_default(),
            "created_at": created_at,
        }),
        tok,
    ).await;
    Ok(KnowledgeItem { item_id, vault_id, session_id, title, source_refs, ai_summary, created_at })
}

/// List all knowledge items for the current vault, newest first.
#[tauri::command]
pub async fn list_knowledge_items(
    state: State<'_, AppState>,
) -> Result<Vec<KnowledgeItem>, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(vec![]); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!("/vaults/{}/kb/items", urlencoding::encode(&vault_id));
    let result: serde_json::Value = daemon_get(&state.http_client, &path, tok)
        .await
        .unwrap_or(serde_json::json!([]));
    let arr = result.as_array().cloned().unwrap_or_default();
    let items = arr.iter().filter_map(|v| parse_knowledge_item(v)).collect();
    Ok(items)
}

/// Get a single knowledge item by item_id.
#[tauri::command]
pub async fn get_knowledge_item(
    state: State<'_, AppState>,
    item_id: String,
) -> Result<KnowledgeItem, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() {
        return Err(AppError::Import("no vault".to_string()));
    }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!(
        "/vaults/{}/kb/items/{}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&item_id),
    );
    let result: serde_json::Value = daemon_get(&state.http_client, &path, tok)
        .await
        .map_err(|e| AppError::Import(e))?;
    parse_knowledge_item(&result).ok_or_else(|| AppError::Import(format!("invalid item: {}", item_id)))
}

/// Delete a knowledge item and its chunks.
#[tauri::command]
pub async fn delete_knowledge_item(
    state: State<'_, AppState>,
    item_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(()); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!(
        "/vaults/{}/kb/items/{}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&item_id),
    );
    let _ = daemon_delete::<serde_json::Value>(&state.http_client, &path, tok).await;
    Ok(())
}

/// Rename a knowledge item title.
#[tauri::command]
pub async fn rename_knowledge_item(
    state: State<'_, AppState>,
    item_id: String,
    title: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(()); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!(
        "/vaults/{}/kb/items/{}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&item_id),
    );
    let _ = daemon_put::<_, serde_json::Value>(
        &state.http_client,
        &path,
        &serde_json::json!({ "title": title }),
        tok,
    ).await;
    Ok(())
}

/// Parse a daemon JSON value into a KnowledgeItem.
fn parse_knowledge_item(v: &serde_json::Value) -> Option<KnowledgeItem> {
    let item_id = v["item_id"].as_str()?.to_string();
    let vault_id = v["vault_id"].as_str().unwrap_or("").to_string();
    let session_id = v["session_id"].as_str().unwrap_or("").to_string();
    let title = v["title"].as_str().unwrap_or("").to_string();
    let ai_summary = v["ai_summary"].as_str().unwrap_or("").to_string();
    let created_at = v["created_at"].as_i64().unwrap_or(0);
    // source_refs may be stored as JSON string or array
    let source_refs: Vec<KnowledgeRef> = if let Some(arr) = v["source_refs"].as_array() {
        arr.iter().filter_map(|r| {
            Some(KnowledgeRef {
                path: r["path"].as_str()?.to_string(),
                title: r["title"].as_str().unwrap_or("").to_string(),
                excerpt: r["excerpt"].as_str().unwrap_or("").to_string(),
            })
        }).collect()
    } else if let Some(s) = v["source_refs"].as_str() {
        serde_json::from_str(s).unwrap_or_default()
    } else {
        vec![]
    };
    Some(KnowledgeItem { item_id, vault_id, session_id, title, source_refs, ai_summary, created_at })
}

// ── Agent Skills CRUD ─────────────────────────────────────────────────────────

/// 儲存技能規範至 agent_skills，並同步計算 trigger embedding（供向量搜尋）。
#[tauri::command]
pub async fn save_agent_skill(
    _app: AppHandle,
    state: State<'_, AppState>,
    knowledge_item_id: String,
    title: String,
    trigger: String,
    behavior: String,
    tool_calls: Vec<String>,
    injection_mode: Option<String>,
    agent_scope: Option<String>,
    need_tool_chain: Option<bool>,
    tool_chain_order: Option<Vec<String>>,
) -> Result<AgentSkillRecord, AppError> {
    let vault_id = state.get_vault_id().await?;
    let skill_id = uuid::Uuid::new_v4().to_string();
    let mode = injection_mode.as_deref().unwrap_or("passive");
    let mode = if mode == "active" { "active" } else { "passive" };
    let scope = valid_scope(agent_scope.as_deref().unwrap_or("all")).to_string();
    let allowed = [
        "search_vault", "read_note", "list_structure", "list_notes_in_folder",
        "open_note", "create_note", "update_note", "append_to_note",
        "delete_note", "delete_folder", "move_note", "create_folder",
        "plan_announce", "query_memory", "web_search",
        "get_current_datetime", "show_toast",
    ];
    let safe_tools: Vec<String> = tool_calls.into_iter()
        .filter(|t| allowed.contains(&t.as_str()))
        .collect();
    let need_chain = need_tool_chain.unwrap_or(false);
    let safe_chain: Vec<String> = tool_chain_order.unwrap_or_default().into_iter()
        .filter(|t| allowed.contains(&t.as_str()))
        .collect();
    let created_at = chrono::Utc::now().timestamp_millis();
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!("/vaults/{}/skills", urlencoding::encode(&vault_id));
    let _ = daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &path,
        &serde_json::json!({
            "skill_id": skill_id,
            "vault_id": vault_id,
            "knowledge_item_id": knowledge_item_id,
            "title": title,
            "trigger": trigger,
            "behavior": behavior,
            "tool_calls": safe_tools,
            "is_active": true,
            "injection_mode": mode,
            "agent_scope": scope,
            "need_tool_chain": need_chain,
            "tool_chain_order": safe_chain,
            "trigger_count": 0,
            "created_at": created_at,
        }),
        tok,
    ).await;
    Ok(AgentSkillRecord {
        skill_id,
        vault_id,
        knowledge_item_id,
        title,
        trigger,
        behavior,
        tool_calls: safe_tools,
        is_active: true,
        injection_mode: mode.to_string(),
        agent_scope: scope,
        need_tool_chain: need_chain,
        tool_chain_order: safe_chain,
        trigger_count: 0,
        last_triggered_at: None,
        created_at,
    })
}

// ── Skill Usage Stats ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct DailyCount {
    pub date: String,  // "YYYY-MM-DD"
    pub count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SkillTrendStat {
    pub skill_id: String,
    pub title: String,
    pub trigger_count: i64,
    pub daily: Vec<DailyCount>,  // 30 天每日觸發數
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SkillUsageStats {
    pub global_daily: Vec<DailyCount>,       // 30 天全局每日觸發數
    pub top_skills: Vec<SkillTrendStat>,     // trigger_count 前 10 的 skill（含各自 30 天）
    pub active_count: i64,
    pub total_triggers_30d: i64,
}

/// 取得過去 30 天技能使用統計：全局趨勢 + top 10 技能各自趨勢。
#[tauri::command]
pub async fn get_skill_usage_stats(
    state: State<'_, AppState>,
) -> Result<SkillUsageStats, AppError> {
    let today = chrono::Utc::now().date_naive();
    let global_daily: Vec<DailyCount> = (0..30i64).rev()
        .map(|d| DailyCount {
            date: (today - chrono::TimeDelta::days(d)).format("%Y-%m-%d").to_string(),
            count: 0,
        })
        .collect();

    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() {
        return Ok(SkillUsageStats { global_daily, top_skills: vec![], active_count: 0, total_triggers_30d: 0 });
    }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!("/vaults/{}/skills", urlencoding::encode(&vault_id));
    let result: serde_json::Value = daemon_get(&state.http_client, &path, tok)
        .await
        .unwrap_or(serde_json::json!([]));
    let arr = result.as_array().cloned().unwrap_or_default();

    let active_count = arr.iter().filter(|v| v["is_active"].as_bool() == Some(true)).count() as i64;
    let total_triggers_30d: i64 = arr.iter()
        .map(|v| v["trigger_count"].as_i64().unwrap_or(0))
        .sum();

    // Top 10 skills by trigger_count
    let mut sorted = arr.clone();
    sorted.sort_by(|a, b| {
        b["trigger_count"].as_i64().unwrap_or(0)
            .cmp(&a["trigger_count"].as_i64().unwrap_or(0))
    });
    let top_skills: Vec<SkillTrendStat> = sorted.iter().take(10).filter_map(|v| {
        let skill_id = v["skill_id"].as_str()?.to_string();
        let title = v["title"].as_str().unwrap_or("").to_string();
        let trigger_count = v["trigger_count"].as_i64().unwrap_or(0);
        Some(SkillTrendStat {
            skill_id,
            title,
            trigger_count,
            daily: vec![],
        })
    }).collect();

    Ok(SkillUsageStats { global_daily, top_skills, active_count, total_triggers_30d })
}

/// 更新技能規範內容並重算 trigger embedding。
#[tauri::command]
pub async fn update_agent_skill(
    _app: AppHandle,
    state: State<'_, AppState>,
    skill_id: String,
    title: String,
    trigger: String,
    behavior: String,
    tool_calls: Vec<String>,
    injection_mode: Option<String>,
    agent_scope: Option<String>,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(()); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!(
        "/vaults/{}/skills/{}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&skill_id),
    );
    let _ = daemon_put::<_, serde_json::Value>(
        &state.http_client,
        &path,
        &serde_json::json!({
            "title": title,
            "trigger": trigger,
            "behavior": behavior,
            "tool_calls": tool_calls,
            "injection_mode": injection_mode.unwrap_or_else(|| "passive".to_string()),
            "agent_scope": agent_scope.unwrap_or_else(|| "all".to_string()),
            "is_active": true,
        }),
        tok,
    ).await;
    Ok(())
}

/// 列出 vault 中所有技能規範，可選擇只回傳特定知識項目或只回傳啟用中的技能。
#[tauri::command]
pub async fn list_agent_skills(
    state: State<'_, AppState>,
    knowledge_item_id: Option<String>,
    active_only: bool,
) -> Result<Vec<AgentSkillRecord>, AppError> {
    let token = state.get_auth_token().await;
    log::info!("[list_agent_skills] token_len={}", token.len());
    if token.is_empty() { return Ok(vec![]); }
    let tok = Some(token.as_str());
    let path = "/skills";
    let raw = daemon_get::<serde_json::Value>(&state.http_client, path, tok).await;
    log::info!("[list_agent_skills] daemon_get result: {:?}", raw.as_ref().map(|v| v.to_string()).unwrap_or_else(|e| format!("ERR: {}", e)));
    let result = raw.unwrap_or(serde_json::json!([]));
    let arr = result.as_array().cloned().unwrap_or_default();
    let skills = arr
        .iter()
        .filter(|v| {
            if active_only && v["is_active"].as_bool() != Some(true) { return false; }
            if let Some(ref kid) = knowledge_item_id {
                if v["knowledge_item_id"].as_str().unwrap_or("") != kid { return false; }
            }
            true
        })
        .filter_map(|v| parse_agent_skill(v))
        .collect();
    Ok(skills)
}

/// 啟用或停用一個技能規範。
#[tauri::command]
pub async fn toggle_agent_skill(
    state: State<'_, AppState>,
    skill_id: String,
    is_active: bool,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(()); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!(
        "/vaults/{}/skills/{}/toggle",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&skill_id),
    );
    let _ = daemon_patch::<_, serde_json::Value>(
        &state.http_client,
        &path,
        &serde_json::json!({ "is_active": is_active }),
        tok,
    ).await;
    Ok(())
}

/// 刪除一個技能規範。
#[tauri::command]
pub async fn delete_agent_skill(
    state: State<'_, AppState>,
    skill_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(()); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!(
        "/vaults/{}/skills/{}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&skill_id),
    );
    let _ = daemon_delete::<serde_json::Value>(&state.http_client, &path, tok).await;
    Ok(())
}

/// Parse a daemon JSON value into an AgentSkillRecord.
fn parse_agent_skill(v: &serde_json::Value) -> Option<AgentSkillRecord> {
    let skill_id = v["skill_id"].as_str()?.to_string();
    let vault_id = v["vault_id"].as_str().unwrap_or("").to_string();
    let knowledge_item_id = v["knowledge_item_id"].as_str().unwrap_or("").to_string();
    let title = v["title"].as_str().unwrap_or("").to_string();
    let trigger = v["trigger"].as_str().unwrap_or("").to_string();
    let behavior = v["behavior"].as_str().unwrap_or("").to_string();
    let is_active = v["is_active"].as_bool().unwrap_or(true);
    let injection_mode = v["injection_mode"].as_str().unwrap_or("passive").to_string();
    let agent_scope = v["agent_scope"].as_str().unwrap_or("all").to_string();
    let trigger_count = v["trigger_count"].as_i64().unwrap_or(0);
    let last_triggered_at = v["last_triggered_at"].as_i64();
    let created_at = v["created_at"].as_i64().unwrap_or(0);
    // tool_calls may be a native array or JSON string
    let tool_calls: Vec<String> = if let Some(arr) = v["tool_calls"].as_array() {
        arr.iter().filter_map(|t| t.as_str().map(|s| s.to_string())).collect()
    } else if let Some(s) = v["tool_calls"].as_str() {
        serde_json::from_str(s).unwrap_or_default()
    } else {
        vec![]
    };
    let need_tool_chain = v["need_tool_chain"].as_bool().unwrap_or(false);
    let tool_chain_order: Vec<String> = if let Some(arr) = v["tool_chain_order"].as_array() {
        arr.iter().filter_map(|t| t.as_str().map(|s| s.to_string())).collect()
    } else {
        vec![]
    };
    Some(AgentSkillRecord {
        skill_id, vault_id, knowledge_item_id, title, trigger, behavior,
        tool_calls, is_active, injection_mode, agent_scope,
        need_tool_chain, tool_chain_order,
        trigger_count, last_triggered_at, created_at,
    })
}

/// 透過 tool-calling 讓 LLM 自行決定技能規範的 tool_calls（知道全部工具）。
/// LLM 唯一可呼叫的工具是 create_agent_skill，可呼叫 1-2 次。
/// 回傳持久化後的 AgentSkillRecord 列表。
pub async fn generate_skills_via_tool_call(
    app_state: &AppState,
    vault_id: &str,
    item_id: &str,
    knowledge_context: &str,
    now_ms: i64,
) -> Vec<AgentSkillRecord> {
    // 從 vault_tools() 提取所有工具名稱+描述，以文字形式注入 system prompt
    let tools_desc: String = crate::commands::agent::vault_tools()
        .as_array()
        .map(|arr| {
            arr.iter().filter_map(|t| {
                let f = t.get("function")?;
                let name = f.get("name")?.as_str()?;
                let desc = f.get("description")?.as_str().unwrap_or("");
                Some(format!("- **{}**: {}", name, desc.lines().next().unwrap_or(desc)))
            }).collect::<Vec<_>>().join("\n")
        })
        .unwrap_or_default();

    let system_prompt = format!(
        "你是技能規範生成助理。根據以下知識內容，設計 1-2 個 Agent 技能規範。\n\
         \n\
         ## 系統可用工具（tool_calls 只能從此清單選擇）\n\
         {tools_desc}\n\
         \n\
         ## 規則\n\
         - trigger 以「當…時」開頭，明確描述觸發情境\n\
         - behavior 為具體可執行的操作指令（先做A，再做B）\n\
         - tool_calls 只選真正需要的工具，寧少勿多\n\
         - injection_mode：passive（embedding 比對觸發）或 active（永遠注入）\n\
         - agent_scope：all / main / search / write / research / memory\n\
         - 若知識不適合產生技能，不要呼叫工具\n\
         請呼叫 create_agent_skill 建立技能規範（最多 2 次）。"
    );

    // create_agent_skill 是唯一可呼叫的工具
    let callable_tool = serde_json::json!([{
        "type": "function",
        "function": {
            "name": "create_agent_skill",
            "description": "建立新技能規範（預設未啟用，使用者可審核後啟用）",
            "parameters": {
                "type": "object",
                "properties": {
                    "title":          { "type": "string", "description": "技能標題" },
                    "trigger":        { "type": "string", "description": "觸發情境描述，以「當…時」開頭" },
                    "behavior":       { "type": "string", "description": "具體操作指令" },
                    "tool_calls":     { "type": "array", "items": { "type": "string" },
                                        "description": "此技能啟用時注入的工具名稱列表" },
                    "injection_mode": { "type": "string", "enum": ["passive", "active"],
                                        "description": "passive = embedding 比對觸發，active = 永遠注入" },
                    "agent_scope":    { "type": "string",
                                        "enum": ["all","main","search","write","research","memory"],
                                        "description": "適用的 agent 範疇" }
                },
                "required": ["title", "trigger", "behavior"]
            }
        }
    }]);

    let tok_owned = app_state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    let Ok(resp) = crate::api_client::daemon_post::<_, serde_json::Value>(
        &app_state.http_client,
        &format!("/vaults/{}/agent/invoke", urlencoding::encode(vault_id)),
        &serde_json::json!({
            "system":      system_prompt,
            "input":       knowledge_context,
            "tools":       callable_tool,
            "tool_choice": "auto",
            "temperature": 0.3,
            "max_tokens":  1200,
        }),
        tok,
    ).await else { return vec![]; };

    // 解析 tool_calls（LLM 可能呼叫 1-2 次 create_agent_skill）
    let tool_calls = resp["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let mut saved: Vec<AgentSkillRecord> = Vec::new();

    for tc in &tool_calls {
        let fn_name = tc.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or("");
        if fn_name != "create_agent_skill" { continue; }

        let args_str = tc.pointer("/function/arguments").and_then(|v| v.as_str()).unwrap_or("{}");
        let Ok(args) = serde_json::from_str::<serde_json::Value>(args_str) else { continue; };

        let title   = args["title"].as_str().unwrap_or("未命名技能").to_string();
        let trigger = args["trigger"].as_str().unwrap_or("").to_string();
        let behavior= args["behavior"].as_str().unwrap_or("").to_string();
        let mode    = match args["injection_mode"].as_str() {
            Some("active") => "active",
            _              => "passive",
        };
        let scope = valid_scope(args["agent_scope"].as_str().unwrap_or("all")).to_string();

        // 驗證 tool_calls：每個名稱必須存在於 vault_tools()
        let valid_names: std::collections::HashSet<String> = crate::commands::agent::vault_tools()
            .as_array().map(|arr| {
                arr.iter().filter_map(|t| {
                    t.pointer("/function/name")?.as_str().map(|s| s.to_string())
                }).collect()
            }).unwrap_or_default();

        let tool_calls_vec: Vec<String> = args["tool_calls"]
            .as_array()
            .map(|a| a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .filter(|n| valid_names.contains(n))
                .collect())
            .unwrap_or_default();

        let skill_id = uuid::Uuid::new_v4().to_string();
        // Save to daemon (best-effort)
        let daemon_client = reqwest::Client::new();
        let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
            &daemon_client,
            &format!("/vaults/{}/skills", urlencoding::encode(vault_id)),
            &serde_json::json!({
                "skill_id": skill_id,
                "knowledge_item_id": item_id,
                "title": title,
                "trigger": trigger,
                "behavior": behavior,
                "tool_calls": tool_calls_vec,
                "is_active": false,
                "injection_mode": mode,
                "agent_scope": scope,
            }),
            None,
        ).await;

        saved.push(AgentSkillRecord {
            skill_id,
            vault_id: vault_id.to_owned(),
            knowledge_item_id: item_id.to_owned(),
            title,
            trigger,
            behavior,
            tool_calls: tool_calls_vec,
            is_active: false,
            injection_mode: mode.to_string(),
            agent_scope: scope,
            need_tool_chain: false,
            tool_chain_order: vec![],
            trigger_count: 0,
            last_triggered_at: None,
            created_at: now_ms,
        });
    }

    saved
}

// ── 共用：載入知識項目 ─────────────────────────────────────────────────────
/// Returns (item_id, title, user_content) where user_content is formatted for LLM.
async fn load_ki_context(
    vault_id: &str,
    item_id: &str,
) -> Result<(String, String, String), AppError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let url = format!(
        "http://127.0.0.1:7787/api/v1/vaults/{}/kb/items/{}",
        urlencoding::encode(vault_id),
        urlencoding::encode(item_id),
    );
    let resp = client.get(&url).send().await
        .map_err(|e| AppError::Import(format!("load_ki_context fetch failed: {}", e)))?;
    if !resp.status().is_success() {
        return Err(AppError::Import(format!("knowledge item not found: {}", item_id)));
    }
    let json: serde_json::Value = resp.json().await
        .map_err(|e| AppError::Import(format!("load_ki_context parse failed: {}", e)))?;
    let title = json["title"].as_str().unwrap_or("未命名").to_string();
    let ai_summary = json["ai_summary"].as_str().unwrap_or("").to_string();
    let source_refs: Vec<serde_json::Value> = json["source_refs"].as_array()
        .cloned()
        .or_else(|| json["source_refs"].as_str()
            .and_then(|s| serde_json::from_str(s).ok()))
        .unwrap_or_default();
    let refs_text = source_refs.iter().enumerate().map(|(i, r)| {
        format!("[{}] {}: {}", i + 1,
            r["title"].as_str().unwrap_or(""),
            r["excerpt"].as_str().unwrap_or(""))
    }).collect::<Vec<_>>().join("\n");
    let user_content = format!(
        "知識項目標題：{}\n\n摘要：\n{}\n\n來源：\n{}",
        title, ai_summary, refs_text
    );
    Ok((item_id.to_string(), title, user_content))
}

// ── 筆記卡片 system prompt（共用）────────────────────────────────────────────
fn note_card_system_prompt() -> &'static str {
    r#"你是知識管理 AI 助理，根據提供的知識內容，回傳嚴格 JSON（不含其他文字）：

{
  "note_cards": [
    {
      "title": "卡片標題",
      "template": "concept | procedure | reference",
      "content": "完整 markdown（含 frontmatter）",
      "reason": "為什麼值得建立這張卡片"
    }
  ]
}

規則：
- 2-3 張，template 限 concept/procedure/reference
- content frontmatter 格式：---\nstatus: draft\ntags: [concept]\n---

note_cards content 格式範例（concept）：
---
status: draft
tags: [concept]
---

# 標題

## 定義

> 簡短定義

## 詳細說明

（從知識摘要中提取的核心內容）

## 來源

- 原始知識標題"#
}

/// 只生成筆記卡片建議（不觸發技能規範）。
/// 完成後 emit kb:note_cards_ready 事件。
#[tauri::command]
pub async fn suggest_note_cards_for_item(
    app: AppHandle,
    state: State<'_, AppState>,
    item_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let (_, _, user_content) = load_ki_context(&vault_id, &item_id).await?;

    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };

    let resp: serde_json::Value = crate::api_client::daemon_post(
        &state.http_client,
        &format!("/vaults/{}/agent/invoke", urlencoding::encode(&vault_id)),
        &serde_json::json!({
            "system":      note_card_system_prompt(),
            "input":       &user_content,
            "temperature": 0.3,
            "max_tokens":  1500,
        }),
        tok,
    ).await.map_err(|e| AppError::AI(e))?;

    let raw = resp["text"].as_str().unwrap_or("").trim().to_string();
    let obj_start = raw.find('{').unwrap_or(0);
    let obj_end   = raw.rfind('}').map(|i| i + 1).unwrap_or(raw.len());

    #[derive(Deserialize, Default)]
    struct NoteCardsOnly { #[serde(default)] note_cards: Vec<KBCardSuggestion> }
    let parsed: NoteCardsOnly = serde_json::from_str(&raw[obj_start..obj_end])
        .unwrap_or_default();

    // DB writes removed (daemon handles indexing)
    let _ = app.emit("kb:note_cards_ready", serde_json::json!({
        "item_id": &item_id,
        "note_cards": &parsed.note_cards,
    }));
    Ok(())
}

/// 只生成技能規範建議（不觸發筆記卡片）。
/// 完成後 emit kb:skill_cards_ready 事件。
#[tauri::command]
pub async fn suggest_skill_cards_for_item(
    app: AppHandle,
    state: State<'_, AppState>,
    item_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let (_, _, user_content) = load_ki_context(&vault_id, &item_id).await?;
    let now_ms = chrono::Local::now().timestamp_millis();

    let saved_skills = generate_skills_via_tool_call(
        state.inner(), &vault_id, &item_id, &user_content, now_ms,
    ).await;

    let _ = app.emit("kb:skill_cards_ready", serde_json::json!({
        "item_id": &item_id,
        "skill_cards": &saved_skills,
    }));
    Ok(())
}

/// 為知識項目生成 AI 建議卡片（筆記卡片 + 技能規範）。
/// 改為非串流，LLM 回傳結構化 JSON；技能規範自動持久化並計算 embedding。
/// 完成後發出 kb:suggestions_ready 事件。
#[tauri::command]
pub async fn suggest_kb_cards_for_item(
    app: AppHandle,
    state: State<'_, AppState>,
    item_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let (_, _, user_content) = load_ki_context(&vault_id, &item_id).await?;
    let now_ms = chrono::Local::now().timestamp_millis();
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };

    // Generate note cards
    let note_cards: Vec<KBCardSuggestion> = if let Ok(resp) = crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/agent/invoke", urlencoding::encode(&vault_id)),
        &serde_json::json!({
            "system":      note_card_system_prompt(),
            "input":       &user_content,
            "temperature": 0.3,
            "max_tokens":  1500,
        }),
        tok,
    ).await {
        let raw = resp["text"].as_str().unwrap_or("").trim().to_string();
        let obj_start = raw.find('{').unwrap_or(0);
        let obj_end = raw.rfind('}').map(|i| i + 1).unwrap_or(raw.len());
        #[derive(Deserialize, Default)]
        struct NC { #[serde(default)] note_cards: Vec<KBCardSuggestion> }
        serde_json::from_str::<NC>(&raw[obj_start..obj_end]).unwrap_or_default().note_cards
    } else { vec![] };

    // Generate skill cards
    let saved_skills = generate_skills_via_tool_call(
        state.inner(), &vault_id, &item_id, &user_content, now_ms,
    ).await;

    let _ = app.emit("kb:suggestions_ready", serde_json::json!({
        "item_id": &item_id,
        "note_cards": &note_cards,
        "skill_cards": &saved_skills,
    }));
    Ok(())
}

// ── compress_conversation_to_knowledge ───────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct CompressedConvResult {
    pub item_id: String,
    pub title: String,
    pub skill_count: usize,
}

/// LLM 壓縮對話輸出格式
#[derive(Debug, Serialize, Deserialize)]
struct ConvCompression {
    title: String,
    knowledge_summary: String,
    skill_candidates: Vec<AgentSkillSuggestion>,
}

/// 將一段 Chat 對話以 skill-first 方式壓縮為知識項目 + 技能規範。
/// 一次 LLM call 萃取：標題、知識摘要（供向量搜尋）、技能候選（直接可用的行為規則）。
/// 完成後 emit kb:suggestions_ready，前端可跳轉 Import Center 查看。
#[tauri::command]
pub async fn compress_conversation_to_knowledge(
    app: AppHandle,
    state: State<'_, AppState>,
    messages_json: String,
) -> Result<CompressedConvResult, AppError> {
    let vault_id = state.get_vault_id().await?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };

    let compress_system = r#"你是對話知識萃取助理。根據以下對話，回傳嚴格 JSON（不含其他文字）：
{
  "title": "知識標題（20字以內）",
  "knowledge_summary": "核心知識摘要（300字以內，供向量搜尋）",
  "skill_candidates": []
}
skill_candidates 可為空陣列。如有可重用行為規則才填入。"#;

    let resp: serde_json::Value = crate::api_client::daemon_post(
        &state.http_client,
        &format!("/vaults/{}/agent/invoke", urlencoding::encode(&vault_id)),
        &serde_json::json!({
            "system":      compress_system,
            "input":       messages_json,
            "temperature": 0.2,
            "max_tokens":  800,
        }),
        tok,
    ).await.map_err(|e| AppError::AI(e))?;

    let raw = resp["text"].as_str().unwrap_or("").trim().to_string();
    let obj_start = raw.find('{').unwrap_or(0);
    let obj_end = raw.rfind('}').map(|i| i + 1).unwrap_or(raw.len());
    let compressed: ConvCompression = serde_json::from_str(&raw[obj_start..obj_end])
        .unwrap_or(ConvCompression {
            title: "對話壓縮".to_string(),
            knowledge_summary: raw.clone(),
            skill_candidates: vec![],
        });

    let item_id = Uuid::new_v4().to_string();
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    // Save knowledge item to daemon
    let _ = daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/kb/items", urlencoding::encode(&vault_id)),
        &serde_json::json!({
            "item_id": item_id,
            "vault_id": vault_id,
            "session_id": "",
            "title": compressed.title,
            "ai_summary": compressed.knowledge_summary,
            "source_refs": "[]",
            "created_at": now_ms,
        }),
        tok,
    ).await;

    // Generate skill cards from candidate suggestions
    let saved_skills = generate_skills_via_tool_call(
        state.inner(), &vault_id, &item_id, &compressed.knowledge_summary, now_ms,
    ).await;
    let skill_count = saved_skills.len();

    let _ = app.emit("kb:suggestions_ready", serde_json::json!({
        "item_id": &item_id,
        "note_cards": serde_json::json!([]),
        "skill_cards": &saved_skills,
    }));

    Ok(CompressedConvResult {
        item_id,
        title: compressed.title,
        skill_count,
    })
}

// ── extract_skill_from_exchange ───────────────────────────────────────────────

/// 從單次對話交換（使用者訊息 + 助理回覆）萃取一條技能規範建議。
/// 前端可在偵測到「記住」等關鍵字或使用者點擊📌按鈕時呼叫。
#[tauri::command]
pub async fn extract_skill_from_exchange(
    app: AppHandle,
    state: State<'_, AppState>,
    user_msg: String,
    assistant_msg: String,
) -> Result<AgentSkillSuggestion, AppError> {
    let vault_id = state.get_vault_id().await?;
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };

    let system_prompt = r#"你是個人 AI 行為規則萃取器。根據以下對話交換，萃取一條可重用的技能規範。
回傳嚴格 JSON（不含其他文字、不含 markdown code fence）：

{
  "title": "技能標題（10字以內）",
  "trigger": "當使用者...時（具體觸發條件，15-30字）",
  "behavior": "應先...，再...（具體、可執行的操作指令）",
  "tool_calls": []
}

tool_calls 只能包含：search_vault、read_note、list_structure（或空陣列）。
若對話內容無可萃取的行為規則，trigger 欄填入「無法萃取」。"#;

    let conv = format!("使用者：{}\n\n助理：{}", user_msg, assistant_msg);
    let resp: serde_json::Value = crate::api_client::daemon_post(
        &state.http_client,
        &format!("/vaults/{}/agent/invoke", urlencoding::encode(&vault_id)),
        &serde_json::json!({
            "system":      system_prompt,
            "input":       conv,
            "temperature": 0.2,
            "max_tokens":  400,
        }),
        tok,
    ).await.map_err(|e| AppError::AI(e))?;

    let raw = resp["text"].as_str().unwrap_or("").trim().to_string();

    // 尋找 JSON 物件邊界（容錯 markdown code fence）
    let obj_start = raw.find('{').unwrap_or(0);
    let obj_end = raw.rfind('}').map(|i| i + 1).unwrap_or(raw.len());
    let json_str = &raw[obj_start..obj_end];

    let skill: AgentSkillSuggestion = serde_json::from_str(json_str)
        .map_err(|e| AppError::AI(format!("JSON 解析失敗：{}\n原始：{}", e, json_str.chars().take(200).collect::<String>())))?;

    Ok(skill)
}

/// 診斷：列出目前 vault 的 import sessions + pages 狀態 + chunks 分佈
#[tauri::command]
pub async fn debug_kb_chunks(state: State<'_, AppState>) -> Result<String, AppError> {
    let vault_id = state.get_vault_id().await?;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let mut output = format!("=== debug_kb_chunks ===\nvault_id: {}\n\n", vault_id);

    // Sessions
    let sessions_path = format!("/vaults/{}/kb/sessions", urlencoding::encode(&vault_id));
    let sessions_val: serde_json::Value = daemon_get(&state.http_client, &sessions_path, tok)
        .await.unwrap_or(serde_json::json!([]));
    let sessions = sessions_val.as_array().cloned().unwrap_or_default();
    output.push_str(&format!("Sessions: {}\n", sessions.len()));
    for s in &sessions {
        output.push_str(&format!("  - {} [{}]: {} pages total, {} imported\n",
            s["site_name"].as_str().unwrap_or("?"),
            s["status"].as_str().unwrap_or("?"),
            s["total_pages"].as_i64().unwrap_or(0),
            s["imported_pages"].as_i64().unwrap_or(0),
        ));
    }

    // KB items
    let items_path = format!("/vaults/{}/kb/items", urlencoding::encode(&vault_id));
    let items_val: serde_json::Value = daemon_get(&state.http_client, &items_path, tok)
        .await.unwrap_or(serde_json::json!([]));
    let items = items_val.as_array().cloned().unwrap_or_default();
    output.push_str(&format!("\nKnowledge Items: {}\n", items.len()));

    // Stats
    let stats_path = format!("/vaults/{}/kb/stats", urlencoding::encode(&vault_id));
    if let Ok(stats) = daemon_get::<serde_json::Value>(&state.http_client, &stats_path, tok).await {
        output.push_str(&format!("\nStats: notes={}, verified={}, draft={}\n",
            stats["total_notes"].as_i64().unwrap_or(0),
            stats["verified"].as_i64().unwrap_or(0),
            stats["draft"].as_i64().unwrap_or(0),
        ));
    }

    Ok(output)
}

// ── KB Assistant（vault-wide verified-only RAG）────────────────────────────

/// Vault-wide KB Q&A：只搜 verified chunks，嚴格只用知識庫內容回答，附來源引用。
/// Events: knowledge:token, knowledge:refs, knowledge:done
#[tauri::command]
pub async fn query_kb(
    app: AppHandle,
    state: State<'_, AppState>,
    query_id: String,
    question: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() {
        let _ = app.emit("knowledge:done", serde_json::json!({ "query_id": &query_id }));
        return Ok(());
    }
    run_knowledge_query(&app, &vault_id, &question, None, &query_id, state.inner()).await
}

// ── KB Dashboard ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct KBStats {
    pub total_notes: i64,
    pub verified: i64,
    pub draft: i64,
    pub deprecated: i64,
    pub no_status: i64,
    pub topics: Vec<KBTopic>,
    pub daily_trend: Vec<KBDayEntry>,
}

#[derive(Debug, Serialize)]
pub struct KBTopic {
    pub name: String,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct KBDayEntry {
    pub date: String,
    pub total: i64,
    pub verified: i64,
}

/// 回傳知識庫統計：筆記狀態分佈、資料夾主題分佈、最近 30 天每日統計。
#[tauri::command]
pub async fn get_kb_stats(state: State<'_, AppState>) -> Result<KBStats, AppError> {
    use chrono::Local;
    let today = Local::now().date_naive();
    let empty_trend: Vec<KBDayEntry> = (0..30i64).map(|i| {
        let d = today - chrono::TimeDelta::days(29 - i);
        KBDayEntry { date: d.format("%Y-%m-%d").to_string(), total: 0, verified: 0 }
    }).collect();

    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() {
        return Ok(KBStats { total_notes: 0, verified: 0, draft: 0, deprecated: 0, no_status: 0, topics: vec![], daily_trend: empty_trend });
    }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!("/vaults/{}/kb/stats", urlencoding::encode(&vault_id));
    let result: serde_json::Value = daemon_get(&state.http_client, &path, tok)
        .await
        .unwrap_or(serde_json::json!({}));

    let daily_trend: Vec<KBDayEntry> = result["daily_trend"].as_array()
        .map(|arr| arr.iter().map(|v| KBDayEntry {
            date: v["date"].as_str().unwrap_or("").to_string(),
            total: v["total"].as_i64().unwrap_or(0),
            verified: v["verified"].as_i64().unwrap_or(0),
        }).collect())
        .unwrap_or(empty_trend);

    let topics: Vec<KBTopic> = result["topics"].as_array()
        .map(|arr| arr.iter().filter_map(|v| {
            Some(KBTopic {
                name: v["name"].as_str()?.to_string(),
                count: v["count"].as_i64().unwrap_or(0),
            })
        }).collect())
        .unwrap_or_default();

    Ok(KBStats {
        total_notes: result["total_notes"].as_i64().unwrap_or(0),
        verified: result["verified"].as_i64().unwrap_or(0),
        draft: result["draft"].as_i64().unwrap_or(0),
        deprecated: result["deprecated"].as_i64().unwrap_or(0),
        no_status: result["no_status"].as_i64().unwrap_or(0),
        topics,
        daily_trend,
    })
}

// ── Knowledge Aging ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AgingNote {
    pub file_path: String,
    pub title: String,
    pub days_since_review: i64,
    pub reviewed_at: Option<i64>,
}

/// 回傳已驗證但超過 threshold_days 天未審查的筆記列表。
/// Scans vault .md files for frontmatter `status: verified` + `reviewed_at`.
#[tauri::command]
pub async fn get_aging_notes(
    state: State<'_, AppState>,
    threshold_days: Option<i64>,
) -> Result<Vec<AgingNote>, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() { return Ok(vec![]); }
    let threshold = threshold_days.unwrap_or(30);
    let now_secs = chrono::Utc::now().timestamp();
    let _threshold_secs = threshold * 86400;
    let vault_root = std::path::PathBuf::from(&vault_path);

    let mut results = Vec::new();
    let mut stack = vec![vault_root.clone()];
    while let Some(dir) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("md") { continue; }
            let Ok(content) = tokio::fs::read_to_string(&path).await else { continue };
            // Parse frontmatter
            if !content.starts_with("---") { continue; }
            let fm_end = match content[3..].find("\n---") {
                Some(e) => 3 + e,
                None => continue,
            };
            let fm = &content[3..fm_end];
            // Check status: verified
            let is_verified = fm.lines().any(|l| {
                let l = l.trim();
                l == "status: verified" || l.starts_with("status:") && l.contains("verified")
            });
            if !is_verified { continue; }
            // Parse reviewed_at
            let reviewed_at: Option<i64> = fm.lines().find_map(|l| {
                let l = l.trim();
                if l.starts_with("reviewed_at:") {
                    let val = l["reviewed_at:".len()..].trim();
                    // Try parse as Unix timestamp or YYYY-MM-DD
                    val.parse::<i64>().ok().or_else(|| {
                        chrono::NaiveDate::parse_from_str(val, "%Y-%m-%d").ok()
                            .and_then(|d| d.and_hms_opt(0, 0, 0))
                            .map(|dt| dt.and_utc().timestamp())
                    })
                } else { None }
            });
            let days_since = match reviewed_at {
                Some(ts) => (now_secs - ts) / 86400,
                None => threshold + 1, // no reviewed_at → treat as aging
            };
            if days_since < threshold { continue; }
            // Get title from frontmatter or filename
            let title = fm.lines().find_map(|l| {
                let l = l.trim();
                if l.starts_with("title:") {
                    Some(l["title:".len()..].trim().trim_matches('"').to_string())
                } else { None }
            }).unwrap_or_else(|| {
                path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
            });
            let rel_path = path.strip_prefix(&vault_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            results.push(AgingNote { file_path: rel_path, title, days_since_review: days_since, reviewed_at });
        }
    }
    results.sort_by(|a, b| b.days_since_review.cmp(&a.days_since_review));
    Ok(results)
}

/// 標記筆記為「已審查」（更新 reviewed_at frontmatter 欄位）
#[tauri::command]
pub async fn mark_note_reviewed(
    state: State<'_, AppState>,
    path: String,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() { return Ok(()); }
    let abs_path = std::path::PathBuf::from(&vault_path).join(&path);
    let content = tokio::fs::read_to_string(&abs_path).await?;
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let new_content = if content.starts_with("---") {
        if let Some(fm_end) = content[3..].find("\n---") {
            let fm = &content[3..3 + fm_end];
            let rest = &content[3 + fm_end + 4..]; // after closing ---
            let new_fm = if fm.lines().any(|l| l.trim().starts_with("reviewed_at:")) {
                fm.lines().map(|l| {
                    if l.trim().starts_with("reviewed_at:") {
                        format!("reviewed_at: {}", today)
                    } else { l.to_string() }
                }).collect::<Vec<_>>().join("\n")
            } else {
                format!("{}\nreviewed_at: {}", fm, today)
            };
            format!("---\n{}{}---{}", new_fm, if new_fm.ends_with('\n') { "" } else { "\n" }, rest)
        } else { content }
    } else { content };
    tokio::fs::write(&abs_path, new_content.as_bytes()).await?;

    // Sync to daemon
    if let Ok(vault_id) = state.get_vault_id().await {
        if !vault_id.is_empty() {
            let token = state.get_auth_token().await;
            let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
            let _ = daemon_post::<_, serde_json::Value>(
                &state.http_client,
                &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
                &serde_json::json!({ "path": path, "content": new_content }),
                tok,
            ).await;
        }
    }

    Ok(())
}

// ── Web Search Tool ───────────────────────────────────────────────────────────

const BRAVE_SEARCH_MONTHLY_LIMIT: u32 = 1000;

/// Current month string, e.g. "2026-03".
fn current_month_str() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
}

/// "x月1號" reset label for the *next* month.
fn next_month_reset_label() -> String {
    let now = chrono::Utc::now();
    let next_month = if now.month() == 12 { 1 } else { now.month() + 1 };
    format!("{}月1號", next_month)
}

/// Short identifier derived from API key (first 8 hex chars of SHA-256).
/// Used to namespace per-key counters without storing the key itself.
fn brave_key_id(api_key: &str) -> String {
    sha256_hex(api_key)[..8].to_string()
}

/// How many Brave searches have been used this calendar month for the given API key.
pub async fn get_brave_used(http_client: &reqwest::Client, tok: Option<&str>, key_id: &str) -> u32 {
    let month_key = format!("brave_search_month_{}", key_id);
    let used_key  = format!("brave_search_used_{}", key_id);
    let stored_month = crate::api_client::daemon_get_setting(http_client, tok, &month_key)
        .await.unwrap_or_default();
    if stored_month != current_month_str() {
        return 0;
    }
    crate::api_client::daemon_get_setting(http_client, tok, &used_key)
        .await
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Increment the monthly Brave search counter for the given API key,
/// resetting automatically when the month changes.
async fn increment_brave_used(http_client: &reqwest::Client, tok: Option<&str>, key_id: &str) {
    let month_key = format!("brave_search_month_{}", key_id);
    let used_key  = format!("brave_search_used_{}", key_id);
    let month = current_month_str();
    let stored_month = crate::api_client::daemon_get_setting(http_client, tok, &month_key)
        .await.unwrap_or_default();
    let new_used = if stored_month != month {
        crate::api_client::daemon_set_setting(http_client, tok, &month_key, &month).await;
        1u32
    } else {
        get_brave_used(http_client, tok, key_id).await + 1
    };
    crate::api_client::daemon_set_setting(http_client, tok, &used_key, &new_used.to_string()).await;
}

/// Read Brave Search API key from daemon settings (decrypted).
async fn read_brave_api_key(http_client: &reqwest::Client, tok: Option<&str>) -> Option<String> {
    let enc = crate::api_client::daemon_get_setting(http_client, tok, "api_key_brave_search")
        .await.unwrap_or_default();
    let plain = crate::crypto::decrypt_api_key(&enc);
    if plain.is_empty() { None } else { Some(plain) }
}

/// Search via Brave Search API. Returns Ok(results) or Err(human-readable reason).
async fn brave_search(api_key: &str, query: &str) -> Result<Vec<(String, String, String)>, String> {
    #[derive(serde::Deserialize)]
    struct BraveResult {
        title: Option<String>,
        url: Option<String>,
        description: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct BraveWeb {
        results: Option<Vec<BraveResult>>,
    }
    #[derive(serde::Deserialize)]
    struct BraveResponse {
        web: Option<BraveWeb>,
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("建立 HTTP client 失敗：{}", e))?;

    // Do NOT send Accept-Encoding: gzip — reqwest lacks the gzip feature,
    // so manually requesting gzip would result in undecodable responses.
    let response = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .query(&[("q", query), ("count", "5")])
        .send()
        .await
        .map_err(|e| format!("網路請求失敗：{}", e))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Brave API 回傳 HTTP {}：{}", status, body));
    }

    let resp: BraveResponse = response.json().await
        .map_err(|e| format!("解析 Brave API 回應失敗：{}", e))?;

    let results = resp.web
        .and_then(|w| w.results)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let title = r.title.unwrap_or_default();
            let url = r.url.unwrap_or_default();
            let snippet = r.description.unwrap_or_default();
            if url.is_empty() { None } else { Some((title, url, snippet)) }
        })
        .collect();

    Ok(results)
}

/// Import search result URLs in the background into a new import session.
async fn background_import_search_results(
    vault_id: String,
    query: String,
    urls: Vec<(String, String)>, // (url, title)
    app: AppHandle,
    _emb_url: Option<String>,
) {
    if urls.is_empty() { return; }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    // Create import session for the search query
    let session_id = Uuid::new_v4().to_string();
    let site_name = format!("搜尋：{}", query.chars().take(20).collect::<String>());
    let sanitized = query.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let root_folder = format!("imports/search-{}", &sanitized.chars().take(20).collect::<String>());
    let now_ms = chrono::Utc::now().timestamp_millis();

    let session_url = format!(
        "http://127.0.0.1:7787/api/v1/vaults/{}/kb/sessions",
        urlencoding::encode(&vault_id)
    );
    let _ = client.post(&session_url).json(&serde_json::json!({
        "session_id": session_id,
        "vault_id": vault_id,
        "seed_url": urls.first().map(|(u, _)| u.as_str()).unwrap_or(""),
        "site_name": site_name,
        "root_folder": root_folder,
        "status": "active",
        "auto_update": false,
        "created_at": now_ms,
    })).send().await;

    // Add pages
    let pages: Vec<serde_json::Value> = urls.iter().enumerate().map(|(i, (url, title))| {
        serde_json::json!({
            "page_id": Uuid::new_v4().to_string(),
            "session_id": session_id,
            "url": url,
            "title": title,
            "parent_url": null,
            "depth": i,
            "status": "pending",
        })
    }).collect();
    let pages_url = format!(
        "http://127.0.0.1:7787/api/v1/vaults/{}/kb/sessions/{}/pages",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&session_id),
    );
    let _ = client.post(&pages_url).json(&serde_json::json!({ "pages": pages })).send().await;

    let _ = app.emit("kb:search_import_ready", serde_json::json!({
        "session_id": session_id,
        "query": query,
        "page_count": urls.len(),
    }));
}

/// Web search tool called by the Agent.
/// Searches via Brave Search API.
pub async fn tool_web_search(
    http_client: &reqwest::Client,
    auth_token: &str,
    _vault_id: &str,
    query: &str,
    app: &AppHandle,
    _emb_url: Option<&str>,
) -> String {
    let tok = if auth_token.is_empty() { None } else { Some(auth_token) };
    let api_key = match read_brave_api_key(http_client, tok).await {
        Some(k) => k,
        None => return "請至設定頁面設定 Brave Search API Key".to_string(),
    };
    let key_id = brave_key_id(&api_key);

    // Check monthly quota before making the request
    let used = get_brave_used(http_client, tok, &key_id).await;
    if used >= BRAVE_SEARCH_MONTHLY_LIMIT {
        return format!(
            "已達每月搜尋上限（{}/{}），{}重置。",
            used, BRAVE_SEARCH_MONTHLY_LIMIT, next_month_reset_label()
        );
    }

    let results = match brave_search(&api_key, query).await {
        Ok(r) => r,
        Err(e) => return format!("Brave Search 請求失敗：{}", e),
    };

    // Increment counter on successful response
    if !results.is_empty() {
        increment_brave_used(http_client, tok, &key_id).await;
    }

    if results.is_empty() {
        return format!(
            "Brave Search 未回傳「{}」的搜尋結果（回應成功但結果為空）。",
            query
        );
    }

    // Emit web refs for frontend "儲存為知識" button
    {
        let refs: Vec<serde_json::Value> = results.iter().take(3)
            .map(|(title, url, snippet)| serde_json::json!({"path": url, "title": title, "excerpt": snippet}))
            .collect();
        let _ = app.emit("agent:web_refs", serde_json::json!(refs));
    }

    // Format results for LLM
    let formatted = results
        .iter()
        .enumerate()
        .map(|(i, (title, url, snippet))| {
            format!("[{}] **{}**\n{}\n來源：{}", i + 1, title, snippet, url)
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    format!(
        "搜尋「{}」的結果：\n\n{}",
        query, formatted
    )
}

// ── Cached Page Viewer ────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct CachedPage {
    pub title: String,
    pub url: String,
    pub content_md: String,
}

/// Fetch cached page content from import_pages by URL, for in-app viewer.
#[tauri::command]
pub async fn get_cached_page(
    state: State<'_, AppState>,
    source_url: String,
) -> Result<Option<CachedPage>, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(None); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    // Search via sessions list then pages
    let sessions_path = format!("/vaults/{}/kb/sessions", urlencoding::encode(&vault_id));
    let sessions_val: serde_json::Value = daemon_get(&state.http_client, &sessions_path, tok)
        .await.unwrap_or(serde_json::json!([]));
    let sessions = sessions_val.as_array().cloned().unwrap_or_default();
    for s in sessions {
        let sid = match s["session_id"].as_str() { Some(id) => id.to_string(), None => continue };
        let pages_path = format!(
            "/vaults/{}/kb/sessions/{}/pages",
            urlencoding::encode(&vault_id),
            urlencoding::encode(&sid),
        );
        let pages_val: serde_json::Value = daemon_get(&state.http_client, &pages_path, tok)
            .await.unwrap_or(serde_json::json!([]));
        let pages = pages_val.as_array().cloned().unwrap_or_default();
        if let Some(page) = pages.iter().find(|p| p["url"].as_str() == Some(&source_url)) {
            let content_md = page["content_md"].as_str().unwrap_or("").to_string();
            if !content_md.is_empty() {
                return Ok(Some(CachedPage {
                    title: page["title"].as_str().unwrap_or("").to_string(),
                    url: source_url,
                    content_md,
                }));
            }
        }
    }
    Ok(None)
}

// ── Brave Search Usage ────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct BraveUsageInfo {
    pub used: u32,
    pub limit: u32,
    pub reset_label: String, // e.g. "4月1號"
}

/// Called by the frontend after saving a new Brave API key.
/// Computes the key_id and persists it in DB so future usage queries don't need keychain access.
#[tauri::command]
pub async fn sync_brave_key_id(state: State<'_, AppState>, key: String) -> Result<(), AppError> {
    let kid = if key.is_empty() { String::new() } else { brave_key_id(&key) };
    let tok = state.get_auth_token().await;
    let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
    crate::api_client::daemon_set_setting(&state.http_client, tok_ref, "brave_current_key_id", &kid).await;
    Ok(())
}

#[tauri::command]
pub async fn get_brave_search_usage(state: State<'_, AppState>) -> Result<BraveUsageInfo, AppError> {
    // Read key_id from daemon — no keychain access needed here
    let tok = state.get_auth_token().await;
    let tok_ref = if tok.is_empty() { None } else { Some(tok.as_str()) };
    let key_id = crate::api_client::daemon_get_setting(&state.http_client, tok_ref, "brave_current_key_id")
        .await.unwrap_or_default();
    let used = if key_id.is_empty() { 0 } else { get_brave_used(&state.http_client, tok_ref, &key_id).await };
    Ok(BraveUsageInfo {
        used,
        limit: BRAVE_SEARCH_MONTHLY_LIMIT,
        reset_label: next_month_reset_label(),
    })
}
