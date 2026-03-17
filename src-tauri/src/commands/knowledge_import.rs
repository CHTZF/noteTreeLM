use crate::{commands::ai::{ensure_server_running, read_api_key_sync, get_embedding}, db::{queries, surreal::SurrealDb}, error::AppError, state::AppState};
use futures_util::StreamExt as _;
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
    created_at: surrealdb::sql::Datetime,
}

#[derive(Deserialize)]
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
    last_crawled: Option<surrealdb::sql::Datetime>,
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
            last_crawled: r.last_crawled.map(|dt| dt.timestamp_millis()),
        }
    }
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
    let vault_id = state.get_vault_id().await?;

    let site_name = parsed.host_str().unwrap_or("unknown").to_string();
    let sanitized_domain = site_name.replace('.', "-");
    let root_folder = format!("imports/{}", sanitized_domain);
    let session_id = Uuid::new_v4().to_string();

    let db = &state.db;
    let vid = vault_id.to_owned();
    let sid = session_id.to_owned();
    let seed = seed_url.to_owned();
    let sname = site_name.to_owned();
    let rfolder = root_folder.to_owned();

    let created_at_ms = chrono::Utc::now().timestamp_millis();

    db.query(
        "INSERT INTO import_sessions (vault_id, session_id, conversation_id, seed_url, site_name, root_folder, status, created_at, updated_at) \
         VALUES ($vid, $sid, '', $seed, $sname, $rfolder, 'active', time::now(), time::now())"
    )
    .bind(("vid", vid))
    .bind(("sid", sid))
    .bind(("seed", seed))
    .bind(("sname", sname))
    .bind(("rfolder", rfolder))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(ImportSession {
        session_id,
        seed_url,
        site_name,
        root_folder,
        status: "active".to_string(),
        created_at: created_at_ms,
    })
}

/// List all import sessions for the current vault.
#[tauri::command]
pub async fn list_import_sessions(
    state: State<'_, AppState>,
) -> Result<Vec<ImportSessionSummary>, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let vid = vault_id.to_owned();

    let mut resp = db
        .query(
            "SELECT session_id, seed_url, site_name, root_folder, status, created_at \
             FROM import_sessions WHERE vault_id = $vid ORDER BY created_at DESC",
        )
        .bind(("vid", vid.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let sessions: Vec<SessionRow> = resp.take(0).unwrap_or_default();

    let mut summaries = Vec::new();
    for s in sessions {
        // Count pages
        #[derive(Deserialize)]
        struct CountRow {
            total: i64,
        }

        let sid = s.session_id.to_owned();

        let mut total_resp = db
            .query("SELECT count() AS total FROM import_pages WHERE vault_id = $vid AND session_id = $sid GROUP ALL")
            .bind(("vid", vid.to_owned()))
            .bind(("sid", sid.to_owned()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let total_rows: Vec<CountRow> = total_resp.take(0).unwrap_or_default();
        let total_pages = total_rows.first().map(|r| r.total).unwrap_or(0);

        let mut imp_resp = db
            .query("SELECT count() AS total FROM import_pages WHERE vault_id = $vid AND session_id = $sid AND status = 'imported' GROUP ALL")
            .bind(("vid", vid.to_owned()))
            .bind(("sid", sid.to_owned()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let imp_rows: Vec<CountRow> = imp_resp.take(0).unwrap_or_default();
        let imported_pages = imp_rows.first().map(|r| r.total).unwrap_or(0);

        summaries.push(ImportSessionSummary {
            session_id: s.session_id,
            seed_url: s.seed_url,
            site_name: s.site_name,
            root_folder: s.root_folder,
            status: s.status,
            created_at: s.created_at.timestamp_millis(),
            total_pages,
            imported_pages,
        });
    }

    Ok(summaries)
}

/// Delete an import session and all its pages.
#[tauri::command]
pub async fn delete_import_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let vid = vault_id.to_owned();
    let sid = session_id.to_owned();

    db.query("DELETE import_pages WHERE vault_id = $vid AND session_id = $sid")
        .bind(("vid", vid.to_owned()))
        .bind(("sid", sid.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    db.query("DELETE import_sessions WHERE vault_id = $vid AND session_id = $sid")
        .bind(("vid", vid))
        .bind(("sid", sid))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(())
}

/// Toggle auto_update for an import session.
#[tauri::command]
pub async fn set_session_auto_update(
    state: State<'_, AppState>,
    session_id: String,
    auto_update: bool,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    state.db
        .query("UPDATE import_sessions SET auto_update = $v WHERE vault_id = $vid AND session_id = $sid")
        .bind(("v", auto_update))
        .bind(("vid", vault_id))
        .bind(("sid", session_id))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// Called on app startup: check_page_updates for sessions with auto_update = true.
/// Emits `import:updates_available { session_id, count }` for each session with changes.
pub async fn auto_check_all_sessions(app: &AppHandle, state: &AppState) {
    let vault_id = match state.get_vault_id().await {
        Ok(v) if !v.is_empty() => v,
        _ => return,
    };

    #[derive(serde::Deserialize)]
    struct SessionRow { session_id: String }

    let mut resp = match state.db
        .query("SELECT session_id FROM import_sessions WHERE vault_id = $vid AND auto_update = true")
        .bind(("vid", vault_id.clone()))
        .await
    {
        Ok(r) => r,
        Err(_) => return,
    };

    let sessions: Vec<SessionRow> = resp.take(0).unwrap_or_default();
    if sessions.is_empty() { return; }

    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; noteTreeLM/0.1; knowledge-import)")
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };

    for s in sessions {
        let sid = s.session_id.clone();
        let app2 = app.clone();
        let db2 = state.db.clone();
        let vid2 = vault_id.clone();
        let client2 = client.clone();

        tokio::spawn(async move {
            #[derive(serde::Deserialize)]
            struct PageRow { url: String, content_hash: Option<String>, http_etag: Option<String> }

            let mut resp = match db2.query(
                "SELECT url, content_hash, http_etag FROM import_pages
                 WHERE vault_id = $vid AND session_id = $sid AND status = 'imported'"
            )
            .bind(("vid", vid2.clone()))
            .bind(("sid", sid.clone()))
            .await
            {
                Ok(r) => r,
                Err(_) => return,
            };

            let pages: Vec<PageRow> = resp.take(0).unwrap_or_default();
            let mut changed = 0usize;

            for page in pages {
                if let Ok(head) = client2.head(&page.url).send().await {
                    let cur_etag = head.headers()
                        .get("etag")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    if let (Some(stored), Some(cur)) = (&page.http_etag, &cur_etag) {
                        if stored == cur { continue; }
                    }
                    changed += 1;
                }
            }

            if changed > 0 {
                let _ = app2.emit("import:updates_available", serde_json::json!({
                    "session_id": sid,
                    "count": changed,
                }));
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
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let vid = vault_id.to_owned();
    let sid = session_id.to_owned();

    // Load session to get seed_url
    let mut resp = db
        .query(
            "SELECT session_id, seed_url, site_name, root_folder, status, created_at \
             FROM import_sessions WHERE vault_id = $vid AND session_id = $sid LIMIT 1",
        )
        .bind(("vid", vid.to_owned()))
        .bind(("sid", sid.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let sessions: Vec<SessionRow> = resp.take(0).unwrap_or_default();
    let session = sessions.into_iter().next().ok_or_else(|| {
        AppError::Import(format!("Session not found: {}", session_id))
    })?;

    let seed_url = session.seed_url;
    let parsed_seed = validate_url(&seed_url)?;

    // Fetch seed page
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; noteTreeLM/0.1; knowledge-import)")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;

    let response = client
        .get(parsed_seed.as_str())
        .send()
        .await
        .map_err(|e| AppError::Import(e.to_string()))?;

    let html = response
        .text()
        .await
        .map_err(|e| AppError::Import(e.to_string()))?;

    // Extract title for seed page
    let seed_title = extract_title(&html);
    let seed_title = if seed_title.is_empty() {
        parsed_seed.host_str().unwrap_or("Home").to_string()
    } else {
        seed_title
    };

    // Extract links
    let links = extract_links(&html, &parsed_seed);

    // Deduplicate URLs
    let mut seen_urls = std::collections::HashSet::new();
    seen_urls.insert(seed_url.clone());

    // Collect pages to insert: (url, title, parent_url, depth)
    let mut pages_to_insert: Vec<(String, String, Option<String>, i64)> = Vec::new();
    pages_to_insert.push((seed_url.clone(), seed_title, None, 0));

    for (url, link_text) in links {
        if seen_urls.insert(url.clone()) {
            let title = if link_text.is_empty() {
                url.clone()
            } else {
                link_text
            };
            pages_to_insert.push((url, title, Some(seed_url.clone()), 1));
        }
    }

    // Insert pages into DB (skip if URL already exists)
    for (url, title, parent_url, depth) in &pages_to_insert {
        let page_id = Uuid::new_v4().to_string();
        db.query(
            "INSERT INTO import_pages (vault_id, page_id, session_id, url, title, parent_url, depth, status, created_at) \
             VALUES ($vid, $pid, $sid, $url, $title, $parent_url, $depth, 'pending', time::now()) \
             ON DUPLICATE KEY UPDATE title = $title"
        )
        .bind(("vid", vid.to_owned()))
        .bind(("pid", page_id))
        .bind(("sid", sid.to_owned()))
        .bind(("url", url.to_owned()))
        .bind(("title", title.to_owned()))
        .bind(("parent_url", parent_url.to_owned()))
        .bind(("depth", *depth))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    }

    // Return all pages for this session
    get_session_pages(state, session_id).await
}

/// Import a single page as a markdown note.
#[tauri::command]
pub async fn import_page(
    state: State<'_, AppState>,
    session_id: String,
    page_id: String,
) -> Result<ImportPageResult, AppError> {
    let vault_id = state.get_vault_id().await?;
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }
    let db = &state.db;
    let vid = vault_id.to_owned();
    let sid = session_id.to_owned();
    let pid = page_id.to_owned();

    // Load page record
    let mut page_resp = db
        .query(
            "SELECT page_id, session_id, url, title, parent_url, depth, note_path, content_hash, http_etag, status, last_crawled \
             FROM import_pages WHERE vault_id = $vid AND page_id = $pid LIMIT 1",
        )
        .bind(("vid", vid.to_owned()))
        .bind(("pid", pid.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let page_rows: Vec<PageRow> = page_resp.take(0).unwrap_or_default();
    let page = page_rows.into_iter().next().ok_or_else(|| {
        AppError::Import(format!("Page not found: {}", page_id))
    })?;

    // Load session to get root_folder
    let mut sess_resp = db
        .query(
            "SELECT session_id, seed_url, site_name, root_folder, status, created_at \
             FROM import_sessions WHERE vault_id = $vid AND session_id = $sid LIMIT 1",
        )
        .bind(("vid", vid.to_owned()))
        .bind(("sid", sid.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let sess_rows: Vec<SessionRow> = sess_resp.take(0).unwrap_or_default();
    let session = sess_rows.into_iter().next().ok_or_else(|| {
        AppError::Import(format!("Session not found: {}", session_id))
    })?;

    // Fetch the page
    let parsed_url = validate_url(&page.url)?;
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; noteTreeLM/0.1; knowledge-import)")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;

    let response = client
        .get(parsed_url.as_str())
        .send()
        .await
        .map_err(|e| AppError::Import(e.to_string()))?;

    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let html = response
        .text()
        .await
        .map_err(|e| AppError::Import(e.to_string()))?;

    // Extract title
    let title = {
        let t = extract_title(&html);
        if t.is_empty() {
            page.title.clone()
        } else {
            t
        }
    };

    // Convert HTML to markdown
    let body_md = html_to_markdown_rich(&html);

    // Build full note content with frontmatter
    let now_date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let note_content = format!(
        "---\ntitle: {}\nsource: {}\nimported: {}\n---\n\n{}\n",
        title,
        page.url,
        now_date,
        body_md
    );

    // Compute hash
    let new_hash = sha256_hex(&note_content);
    let was_updated = page
        .content_hash
        .as_ref()
        .map(|h| h != &new_hash)
        .unwrap_or(false);

    // Build note path
    let slug = slugify(&title);
    let note_filename = format!("{}.md", slug);
    let rel_note_path = format!("{}/{}", session.root_folder, note_filename);

    // Write to disk
    let abs_dir = std::path::PathBuf::from(&vault_path).join(&session.root_folder);
    tokio::fs::create_dir_all(&abs_dir).await?;
    let abs_note_path = std::path::PathBuf::from(&vault_path).join(&rel_note_path);
    tokio::fs::write(&abs_note_path, note_content.as_bytes()).await?;

    // Sync note to DB
    let now_dt = surrealdb::sql::Datetime::from(chrono::Utc::now());
    db.query(
        "INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at, checksum) \
         VALUES ($vid, $path, $title, $content, $wc, $now, $now, $checksum) \
         ON DUPLICATE KEY UPDATE title = $title, content = $content, word_count = $wc, modified_at = $now, checksum = $checksum"
    )
    .bind(("vid", vid.to_owned()))
    .bind(("path", rel_note_path.to_owned()))
    .bind(("title", title.to_owned()))
    .bind(("content", note_content.to_owned()))
    .bind(("wc", note_content.split_whitespace().count() as i64))
    .bind(("now", now_dt))
    .bind(("checksum", new_hash.to_owned()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    // Update import_pages record
    let now_dt2 = surrealdb::sql::Datetime::from(chrono::Utc::now());
    let hash_val = new_hash.to_owned();
    let path_val = rel_note_path.to_owned();

    if let Some(etag_val) = etag {
        db.query(
            "UPDATE import_pages SET status = 'imported', note_path = $path, content_hash = $hash, \
             http_etag = $etag, last_crawled = $now WHERE vault_id = $vid AND page_id = $pid"
        )
        .bind(("path", path_val))
        .bind(("hash", hash_val))
        .bind(("etag", etag_val))
        .bind(("now", now_dt2))
        .bind(("vid", vid.to_owned()))
        .bind(("pid", pid))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    } else {
        db.query(
            "UPDATE import_pages SET status = 'imported', note_path = $path, content_hash = $hash, \
             last_crawled = $now WHERE vault_id = $vid AND page_id = $pid"
        )
        .bind(("path", path_val))
        .bind(("hash", hash_val))
        .bind(("now", now_dt2))
        .bind(("vid", vid.to_owned()))
        .bind(("pid", pid))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    }

    Ok(ImportPageResult {
        note_path: rel_note_path,
        title,
        was_updated,
    })
}

/// Check which already-imported pages have updated content.
#[tauri::command]
pub async fn check_page_updates(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<PageUpdateInfo>, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let vid = vault_id.to_owned();
    let sid = session_id.to_owned();

    // Get all imported pages for this session
    let mut resp = db
        .query(
            "SELECT page_id, session_id, url, title, parent_url, depth, note_path, content_hash, http_etag, status, last_crawled \
             FROM import_pages WHERE vault_id = $vid AND session_id = $sid AND status = 'imported'",
        )
        .bind(("vid", vid.to_owned()))
        .bind(("sid", sid))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let pages: Vec<PageRow> = resp.take(0).unwrap_or_default();

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; noteTreeLM/0.1; knowledge-import)")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;

    let mut updates = Vec::new();

    for page in pages {
        let note_path = match &page.note_path {
            Some(p) => p.clone(),
            None => continue,
        };

        // Try HEAD request first with ETag
        let head_result = client.head(&page.url).send().await;
        if let Ok(head_resp) = head_result {
            let current_etag = head_resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            // If ETag matches, skip
            if let (Some(stored_etag), Some(cur_etag)) = (&page.http_etag, &current_etag) {
                if stored_etag == cur_etag {
                    continue;
                }
            }
        }

        // Fetch full content
        let response = match client.get(&page.url).send().await {
            Ok(r) => r,
            Err(_) => continue,
        };

        let html = match response.text().await {
            Ok(h) => h,
            Err(_) => continue,
        };

        let title = {
            let t = extract_title(&html);
            if t.is_empty() { page.title.clone() } else { t }
        };

        let body_md = html_to_markdown_rich(&html);
        let now_date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let new_content = format!(
            "---\ntitle: {}\nsource: {}\nimported: {}\n---\n\n{}\n",
            title, page.url, now_date, body_md
        );

        let new_hash = sha256_hex(&new_content);

        // Compare with stored hash
        let has_changed = page
            .content_hash
            .as_ref()
            .map(|h| h != &new_hash)
            .unwrap_or(true);

        if has_changed {
            updates.push(PageUpdateInfo {
                page_id: page.page_id,
                url: page.url,
                title,
                note_path,
                new_content,
            });
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
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let vid = vault_id.to_owned();
    let sid = session_id.to_owned();

    let mut resp = db
        .query(
            "SELECT page_id, session_id, url, title, parent_url, depth, note_path, content_hash, http_etag, status, last_crawled \
             FROM import_pages WHERE vault_id = $vid AND session_id = $sid ORDER BY depth ASC, title ASC",
        )
        .bind(("vid", vid))
        .bind(("sid", sid))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let rows: Vec<PageRow> = resp.take(0).unwrap_or_default();
    Ok(rows.into_iter().map(ImportPage::from).collect())
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
    let vault_id = state.get_vault_id().await?;
    let db = state.db.clone();
    let app_state = state.inner().clone();
    let qid = query_id.clone();
    let app_clone = app.clone();

    tokio::spawn(async move {
        if let Err(e) = run_knowledge_query(
            &app_clone, &db, &vault_id, &question, session_id.as_deref(), &qid, &app_state,
        ).await {
            let _ = app_clone.emit("knowledge:done", serde_json::json!({
                "query_id": &qid,
                "error": e.to_string()
            }));
        }
    });

    Ok(())
}

async fn run_knowledge_query(
    app: &AppHandle,
    db: &SurrealDb,
    vault_id: &str,
    question: &str,
    session_id: Option<&str>,
    query_id: &str,
    app_state: &AppState,
) -> Result<(), AppError> {
    // 1. Determine path prefix
    let path_prefix = if let Some(sid) = session_id {
        #[derive(Deserialize)]
        struct FolderRow { root_folder: String }
        let mut resp = db.query(
            "SELECT root_folder FROM import_sessions WHERE vault_id = $vid AND session_id = $sid LIMIT 1"
        )
        .bind(("vid", vault_id.to_owned()))
        .bind(("sid", sid.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<FolderRow> = resp.take(0).unwrap_or_default();
        rows.into_iter().next().map(|r| r.root_folder).unwrap_or_else(|| "imports".to_string())
    } else {
        "imports".to_string()
    };

    // 2. BM25 search
    #[derive(Deserialize)]
    struct NoteSearchRow { path: String, title: String, content: String }

    let prefix_slash = format!("{}/", path_prefix);
    #[derive(Deserialize)]
    struct NoteSearchRowScored { path: String, title: String, content: String, #[allow(dead_code)] score: Option<f64> }

    let scored_notes: Vec<NoteSearchRowScored> = db
        .query(
            "SELECT path, title, content, search::score(1) AS score FROM notes \
             WHERE vault_id = $vid AND string::starts_with(path, $prefix) \
             AND (title @1@ $q OR content @1@ $q) \
             ORDER BY score DESC LIMIT 6",
        )
        .bind(("vid", vault_id.to_owned()))
        .bind(("prefix", prefix_slash.clone()))
        .bind(("q", question.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?
        .take(0)
        .unwrap_or_default();

    let mut notes: Vec<NoteSearchRow> = scored_notes.into_iter().map(|r| NoteSearchRow { path: r.path, title: r.title, content: r.content }).collect();

    // Fallback: most recent notes if FTS found nothing
    if notes.is_empty() {
        #[derive(Deserialize)]
        struct NoteRecentRow { path: String, title: String, content: String, #[allow(dead_code)] modified_at: Option<surrealdb::sql::Datetime> }

        let recent: Vec<NoteRecentRow> = db
            .query(
                "SELECT path, title, content, modified_at FROM notes \
                 WHERE vault_id = $vid AND string::starts_with(path, $prefix) \
                 ORDER BY modified_at DESC LIMIT 5",
            )
            .bind(("vid", vault_id.to_owned()))
            .bind(("prefix", prefix_slash))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?
            .take(0)
            .unwrap_or_default();

        notes = recent.into_iter().map(|r| NoteSearchRow { path: r.path, title: r.title, content: r.content }).collect();
    }

    if notes.is_empty() {
        let msg = "目前沒有已匯入的知識點，請先新增來源並匯入頁面。";
        let _ = app.emit("knowledge:token", serde_json::json!({ "query_id": query_id, "content": msg }));
        let _ = app.emit("knowledge:done", serde_json::json!({ "query_id": query_id }));
        return Ok(());
    }

    // 3. Build refs (emitted immediately so UI can show sources while LLM streams)
    let refs: Vec<KnowledgeRef> = notes.iter().map(|n| {
        let title = if n.title.is_empty() {
            n.path.split('/').last().unwrap_or("").to_string()
        } else {
            n.title.clone()
        };
        // Strip frontmatter for excerpt
        let body = if n.content.starts_with("---") {
            n.content.splitn(4, "---").nth(2).unwrap_or(&n.content).trim_start().to_string()
        } else {
            n.content.clone()
        };
        let excerpt: String = body.chars().take(180).collect();
        KnowledgeRef { path: n.path.clone(), title, excerpt }
    }).collect();

    let _ = app.emit("knowledge:refs", serde_json::json!({ "query_id": query_id, "refs": refs }));

    // 4. Build RAG context
    let context = notes.iter().enumerate().map(|(i, n)| {
        let body = if n.content.starts_with("---") {
            n.content.splitn(4, "---").nth(2).unwrap_or(&n.content).trim_start().to_string()
        } else {
            n.content.clone()
        };
        let excerpt: String = body.chars().take(1200).collect();
        format!("[{}] 標題：{}\n{}", i + 1, n.title, excerpt)
    }).collect::<Vec<_>>().join("\n\n---\n\n");

    let system = format!(
        "你是知識庫問答助手。根據以下筆記回答使用者問題。\
        如有引用，以 [1][2] 格式標示來源編號。\
        若筆記內容不足，誠實說明。用繁體中文回答。\n\n筆記：\n\n{}",
        context
    );

    // 5. Read AI provider config
    let provider = queries::get_setting(db, "ai_provider")
        .await.unwrap_or_default().unwrap_or_default();
    let model = queries::get_setting(db, "ai_model")
        .await.unwrap_or_default().unwrap_or_default();
    let api_key = read_api_key_sync(&provider);

    // 6. Stream tokens via appropriate provider
    let client = reqwest::Client::new();

    let response = if provider == "anthropic" {
        // Anthropic Messages API
        let body = serde_json::json!({
            "model": if model.is_empty() { "claude-3-5-haiku-20241022".to_string() } else { model },
            "max_tokens": 1024,
            "system": system,
            "stream": true,
            "messages": [{ "role": "user", "content": question }],
        });
        client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| AppError::AI(format!("Anthropic 請求失敗：{}", e)))?
    } else if !provider.is_empty() {
        // OpenAI-compatible external provider
        let base_url = queries::get_setting(db, "ai_base_url")
            .await.unwrap_or_default()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let body = serde_json::json!({
            "model": if model.is_empty() { "gpt-4o".to_string() } else { model },
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": question }
            ],
            "stream": true,
            "temperature": 0.3,
            "max_tokens": 1024,
        });
        let mut req = client
            .post(format!("{}/chat/completions", base_url.trim_end_matches('/')))
            .json(&body)
            .timeout(std::time::Duration::from_secs(120));
        if !api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", api_key));
        }
        req.send().await.map_err(|e| AppError::AI(format!("外部 AI 請求失敗：{}", e)))?
    } else {
        // Local llama-server
        let base_url = ensure_server_running(app_state, app).await?;
        let body = serde_json::json!({
            "model": "local",
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": question }
            ],
            "stream": true,
            "temperature": 0.3,
            "max_tokens": 1024,
        });
        client
            .post(format!("{}/v1/chat/completions", base_url))
            .json(&body)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| AppError::AI(format!("請求 LLM 失敗：{}", e)))?
    };

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(AppError::AI(format!("LLM 回應錯誤 {}：{}", status, text)));
    }

    let mut stream = response.bytes_stream();
    let mut sse_buf = String::new();
    let is_anthropic = provider == "anthropic";

    while let Some(item) = stream.next().await {
        let bytes = item.map_err(|e| AppError::AI(e.to_string()))?;
        sse_buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(end) = sse_buf.find("\n\n") {
            let event = sse_buf[..end].to_string();
            sse_buf = sse_buf[end + 2..].to_string();
            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" { continue; }
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        let content = if is_anthropic {
                            if json["type"] == "content_block_delta" {
                                json["delta"]["text"].as_str().map(|s| s.to_string())
                            } else { None }
                        } else {
                            json["choices"][0]["delta"]["content"].as_str().map(|s| s.to_string())
                        };
                        if let Some(text) = content {
                            if !text.is_empty() {
                                let _ = app.emit("knowledge:token", serde_json::json!({
                                    "query_id": query_id,
                                    "content": text
                                }));
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = app.emit("knowledge:done", serde_json::json!({ "query_id": query_id }));
    Ok(())
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
    let vault_id = state.get_vault_id().await?;
    let db = state.db.clone();
    let app_state = state.inner().clone();
    let qid = query_id.clone();
    let app_clone = app.clone();

    tokio::spawn(async move {
        if let Err(e) = run_kb_query(&app_clone, &db, &vault_id, &question, &qid, &app_state).await {
            let _ = app_clone.emit("knowledge:done", serde_json::json!({
                "query_id": &qid,
                "error": e.to_string()
            }));
        }
    });
    Ok(())
}

async fn run_kb_query(
    app: &AppHandle,
    db: &SurrealDb,
    vault_id: &str,
    question: &str,
    query_id: &str,
    app_state: &AppState,
) -> Result<(), AppError> {
    #[derive(serde::Deserialize)]
    struct ChunkRow { file_path: String, section: String, content: String }

    // 1. Vector search → BM25 → contains（只搜 verified）
    let emb_port = *app_state.embedding_actual_port.lock().await;
    let emb_url = emb_port.map(|p| format!("http://127.0.0.1:{}", p));
    let client = reqwest::Client::new();

    let chunks: Vec<ChunkRow> = if let Some(ref url) = emb_url {
        let qvec = get_embedding(&client, url, question).await;
        if !qvec.is_empty() {
            let mut resp = db.query(
                "SELECT file_path, section, content FROM chunks
                 WHERE vault_id = $vid AND embedding != NONE AND status = 'verified'
                 ORDER BY vector::similarity::cosine(embedding, $qvec) DESC LIMIT 8"
            )
            .bind(("vid", vault_id.to_owned()))
            .bind(("qvec", qvec))
            .await.map_err(|e| AppError::Database(e.to_string()))?;
            let rows: Vec<ChunkRow> = resp.take(0).unwrap_or_default();
            rows
        } else { vec![] }
    } else { vec![] };

    let chunks: Vec<ChunkRow> = if chunks.is_empty() {
        let mut resp = db.query(
            "SELECT file_path, section, content FROM chunks
             WHERE vault_id = $vid AND status = 'verified' AND content @1@ $q
             LIMIT 8"
        )
        .bind(("vid", vault_id.to_owned()))
        .bind(("q", question.to_owned()))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
        resp.take(0).unwrap_or_default()
    } else { chunks };

    let chunks: Vec<ChunkRow> = if chunks.is_empty() {
        let mut resp = db.query(
            "SELECT file_path, section, content FROM chunks
             WHERE vault_id = $vid AND status = 'verified'
               AND string::contains(string::lowercase(content), string::lowercase($q))
             LIMIT 8"
        )
        .bind(("vid", vault_id.to_owned()))
        .bind(("q", question.to_owned()))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
        resp.take(0).unwrap_or_default()
    } else { chunks };

    if chunks.is_empty() {
        let msg = "知識庫中沒有相關的已驗證資料，無法回答此問題。請先匯入並驗證相關知識。";
        let _ = app.emit("knowledge:token", serde_json::json!({ "query_id": query_id, "content": msg }));
        let _ = app.emit("knowledge:done", serde_json::json!({ "query_id": query_id }));
        return Ok(());
    }

    // 2. Build refs (chunk-level, with section anchor)
    let refs: Vec<KnowledgeRef> = chunks.iter().map(|c| {
        let fname = c.file_path.split('/').last().unwrap_or("").trim_end_matches(".md");
        let title = if c.section.is_empty() {
            fname.to_string()
        } else {
            format!("{} § {}", fname, c.section)
        };
        let path = if c.section.is_empty() {
            c.file_path.clone()
        } else {
            format!("{}#{}", c.file_path, c.section)
        };
        let excerpt: String = c.content.chars().take(160).collect();
        KnowledgeRef { path, title, excerpt }
    }).collect();

    let _ = app.emit("knowledge:refs", serde_json::json!({ "query_id": query_id, "refs": refs }));

    // 3. STRICT system prompt
    let context = chunks.iter().enumerate().map(|(i, c)| {
        let loc = if c.section.is_empty() {
            c.file_path.clone()
        } else {
            format!("{} § {}", c.file_path, c.section)
        };
        let excerpt: String = c.content.chars().take(1500).collect();
        format!("[{}] 來源：{}\n{}", i + 1, loc, excerpt)
    }).collect::<Vec<_>>().join("\n\n---\n\n");

    let system = format!(
        "你是嚴格的知識庫問答助手。\
        規則：\
        1. 只能根據以下「知識庫片段」回答，禁止使用訓練資料中的知識。\
        2. 每個陳述必須以 [1]、[2] 等格式標示來源編號。\
        3. 若知識庫片段中找不到答案，必須明確說「知識庫中沒有此資訊」，不得猜測或補充。\
        4. 用繁體中文回答。\n\n知識庫片段：\n\n{}",
        context
    );

    // 4. AI streaming
    let provider = queries::get_setting(db, "ai_provider").await.unwrap_or_default().unwrap_or_default();
    let model = queries::get_setting(db, "ai_model").await.unwrap_or_default().unwrap_or_default();
    let api_key = read_api_key_sync(&provider);

    let response = if provider == "anthropic" {
        let body = serde_json::json!({
            "model": if model.is_empty() { "claude-3-5-haiku-20241022" } else { model.as_str() },
            "max_tokens": 1024, "system": system, "stream": true,
            "messages": [{ "role": "user", "content": question }],
        });
        client.post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body).timeout(std::time::Duration::from_secs(120))
            .send().await.map_err(|e| AppError::AI(e.to_string()))?
    } else if !provider.is_empty() {
        let base_url = queries::get_setting(db, "ai_base_url").await.unwrap_or_default()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let body = serde_json::json!({
            "model": if model.is_empty() { "gpt-4o" } else { model.as_str() },
            "messages": [{ "role": "system", "content": system }, { "role": "user", "content": question }],
            "stream": true, "temperature": 0.1, "max_tokens": 1024,
        });
        let mut req = client.post(format!("{}/chat/completions", base_url.trim_end_matches('/')))
            .json(&body).timeout(std::time::Duration::from_secs(120));
        if !api_key.is_empty() { req = req.header("Authorization", format!("Bearer {}", api_key)); }
        req.send().await.map_err(|e| AppError::AI(e.to_string()))?
    } else {
        let base_url = ensure_server_running(app_state, app).await?;
        let body = serde_json::json!({
            "model": "local",
            "messages": [{ "role": "system", "content": system }, { "role": "user", "content": question }],
            "stream": true, "temperature": 0.1, "max_tokens": 1024,
        });
        client.post(format!("{}/v1/chat/completions", base_url))
            .json(&body).timeout(std::time::Duration::from_secs(120))
            .send().await.map_err(|e| AppError::AI(e.to_string()))?
    };

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(AppError::AI(format!("LLM 回應錯誤 {}：{}", status, text)));
    }

    let mut stream = response.bytes_stream();
    let mut sse_buf = String::new();
    let is_anthropic = provider == "anthropic";

    while let Some(item) = stream.next().await {
        let bytes = item.map_err(|e| AppError::AI(e.to_string()))?;
        sse_buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(end) = sse_buf.find("\n\n") {
            let event = sse_buf[..end].to_string();
            sse_buf = sse_buf[end + 2..].to_string();
            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" { continue; }
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        let content = if is_anthropic {
                            if json["type"] == "content_block_delta" {
                                json["delta"]["text"].as_str().map(|s| s.to_string())
                            } else { None }
                        } else {
                            json["choices"][0]["delta"]["content"].as_str().map(|s| s.to_string())
                        };
                        if let Some(text) = content {
                            if !text.is_empty() {
                                let _ = app.emit("knowledge:token", serde_json::json!({
                                    "query_id": query_id, "content": text
                                }));
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = app.emit("knowledge:done", serde_json::json!({ "query_id": query_id }));
    Ok(())
}
