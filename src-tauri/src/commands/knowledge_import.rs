use crate::{commands::ai::{ensure_server_running, read_api_key, get_embedding}, db::{queries, surreal::SurrealDb}, error::AppError, state::AppState};
use chrono::Datelike as _;
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

/// Delete an import session and all its pages (including chunks and KB suggestions).
#[tauri::command]
pub async fn delete_import_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let vid = vault_id.to_owned();
    let sid = session_id.to_owned();

    // Collect virtual note_paths so we can delete their chunks
    #[derive(serde::Deserialize)]
    struct PathRow { note_path: Option<String> }
    let mut resp = db
        .query("SELECT note_path FROM import_pages WHERE vault_id = $vid AND session_id = $sid")
        .bind(("vid", vid.to_owned()))
        .bind(("sid", sid.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let path_rows: Vec<PathRow> = resp.take(0).unwrap_or_default();
    let note_paths: Vec<String> = path_rows.into_iter().filter_map(|r| r.note_path).collect();

    // Delete chunks for all pages in this session
    for note_path in &note_paths {
        db.query("DELETE chunks WHERE vault_id = $vid AND file_path = $fp")
            .bind(("vid", vid.to_owned()))
            .bind(("fp", note_path.to_owned()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
    }

    // Delete KB suggestions for this session
    db.query("DELETE kb_suggestions WHERE vault_id = $vid AND session_id = $sid")
        .bind(("vid", vid.to_owned()))
        .bind(("sid", sid.to_owned()))
        .await
        .ok(); // non-fatal if table doesn't exist yet

    // Delete pages and session
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
            #[allow(dead_code)]
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

    // ── Populate sitemap_titles immediately (no embedding), return fast ─────
    // Embedding is spawned in background so the command returns in ~1-2s.
    for (url, title, _, depth) in &pages_to_insert {
        let entry_id = Uuid::new_v4().to_string();
        db.query(
            "INSERT INTO sitemap_titles (vault_id, session_id, entry_id, url, title, depth) \
             VALUES ($vid, $sid, $eid, $url, $title, $depth) \
             ON DUPLICATE KEY UPDATE title = $title"
        )
        .bind(("vid", vid.to_owned())).bind(("sid", sid.to_owned()))
        .bind(("eid", entry_id)).bind(("url", url.to_owned()))
        .bind(("title", title.to_owned())).bind(("depth", *depth))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    }

    // Background: embed titles and update sitemap_titles with vectors
    let emb_url: Option<String> = {
        let port = *state.embedding_actual_port.lock().await;
        port.map(|p| format!("http://127.0.0.1:{}", p))
    };
    if let Some(base_url) = emb_url {
        let db2 = db.clone();
        let pages_clone: Vec<(String, String, i64)> = pages_to_insert
            .iter().map(|(u, t, _, d)| (u.clone(), t.clone(), *d)).collect();
        let vid2 = vid.clone();
        let sid2 = sid.clone();
        tokio::spawn(async move {
            let emb_client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default();
            let futs: Vec<_> = pages_clone.iter().map(|(_, title, _)| {
                let c = emb_client.clone();
                let u = base_url.clone();
                let t = title.clone();
                async move {
                    let v = crate::commands::ai::get_embedding(&c, &u, &t).await;
                    if v.is_empty() { None } else { Some(v) }
                }
            }).collect();
            let embeddings = futures::future::join_all(futs).await;
            for ((url, title, depth), emb_opt) in pages_clone.iter().zip(embeddings.into_iter()) {
                if let Some(emb) = emb_opt {
                    let _ = db2.query(
                        "UPDATE sitemap_titles SET embedding = $emb \
                         WHERE vault_id = $vid AND session_id = $sid AND url = $url"
                    )
                    .bind(("vid", vid2.clone())).bind(("sid", sid2.clone()))
                    .bind(("url", url.clone())).bind(("emb", emb))
                    .await;
                }
                let _ = depth; // suppress unused warning
                let _ = title;
            }
        });
    }

    // Return all pages for this session
    get_session_pages(state, session_id).await
}

/// Lightweight on-demand fetch for Q&A: HTTP fetch → markdown → store content_md.
/// No chunking, no embedding, no mutex — just what the RAG query needs.
async fn fetch_page_content_for_qa(
    db: &SurrealDb,
    vault_id: &str,
    page: &PageRow,
) -> Result<(), AppError> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; noteTreeLM/0.1; knowledge-import)")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;

    let response = client
        .get(page.url.as_str())
        .send()
        .await
        .map_err(|e| AppError::Import(e.to_string()))?;

    let html = response.text().await.map_err(|e| AppError::Import(e.to_string()))?;

    let title = {
        let t = extract_title(&html);
        if t.is_empty() { page.title.clone() } else { t }
    };
    let body_md = html_to_markdown_rich(&html);
    let now_date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let content_md = format!(
        "---\ntitle: {}\nsource: {}\nimported: {}\nstatus: verified\n---\n\n{}\n",
        title, page.url, now_date, body_md
    );
    let new_hash = sha256_hex(&content_md);
    let now_dt = surrealdb::sql::Datetime::from(chrono::Utc::now());

    db.query(
        "UPDATE import_pages SET status = 'imported', title = $title, content_md = $content, \
         content_hash = $hash, last_crawled = $now \
         WHERE vault_id = $vid AND page_id = $pid",
    )
    .bind(("title", title))
    .bind(("content", content_md))
    .bind(("hash", new_hash))
    .bind(("now", now_dt))
    .bind(("vid", vault_id.to_owned()))
    .bind(("pid", page.page_id.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(())
}

/// Core import logic — used by the `import_page` Tauri command.
async fn import_page_inner(
    db: &SurrealDb,
    vault_id: &str,
    page: &PageRow,
    root_folder: &str,
    emb_url: Option<&str>,
) -> Result<ImportPageResult, AppError> {
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

    let html = response.text().await.map_err(|e| AppError::Import(e.to_string()))?;

    let title = {
        let t = extract_title(&html);
        if t.is_empty() { page.title.clone() } else { t }
    };

    let body_md = html_to_markdown_rich(&html);
    let now_date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let note_content = format!(
        "---\ntitle: {}\nsource: {}\nimported: {}\nstatus: verified\n---\n\n{}\n",
        title, page.url, now_date, body_md
    );

    let new_hash = sha256_hex(&note_content);
    let was_updated = page.content_hash.as_ref().map(|h| h != &new_hash).unwrap_or(false);

    let slug = slugify(&title);
    let rel_note_path = format!("{}/{}.md", root_folder, slug);

    let now_ms = chrono::Utc::now().timestamp_millis();
    let chunks = crate::vault::chunker::chunk_note(&rel_note_path, &note_content, now_ms);
    let _ = crate::vault::chunker::upsert_chunks(db, vault_id, &chunks, emb_url).await;

    let now_dt = surrealdb::sql::Datetime::from(chrono::Utc::now());
    let vid = vault_id.to_owned();
    let pid = page.page_id.clone();

    if let Some(etag_val) = etag {
        db.query(
            "UPDATE import_pages SET status = 'imported', note_path = $path, content_md = $content, \
             content_hash = $hash, http_etag = $etag, last_crawled = $now \
             WHERE vault_id = $vid AND page_id = $pid",
        )
        .bind(("path", rel_note_path.clone())).bind(("content", note_content))
        .bind(("hash", new_hash)).bind(("etag", etag_val))
        .bind(("now", now_dt)).bind(("vid", vid)).bind(("pid", pid))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    } else {
        db.query(
            "UPDATE import_pages SET status = 'imported', note_path = $path, content_md = $content, \
             content_hash = $hash, last_crawled = $now \
             WHERE vault_id = $vid AND page_id = $pid",
        )
        .bind(("path", rel_note_path.clone())).bind(("content", note_content))
        .bind(("hash", new_hash)).bind(("now", now_dt)).bind(("vid", vid)).bind(("pid", pid))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    }

    Ok(ImportPageResult { note_path: rel_note_path, title, was_updated })
}

/// Import a single page — stores content in DB only (no vault file written).
#[tauri::command]
pub async fn import_page(
    state: State<'_, AppState>,
    session_id: String,
    page_id: String,
) -> Result<ImportPageResult, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let vid = vault_id.to_owned();

    let mut page_resp = db
        .query(
            "SELECT page_id, session_id, url, title, parent_url, depth, note_path, content_hash, http_etag, status, last_crawled \
             FROM import_pages WHERE vault_id = $vid AND page_id = $pid LIMIT 1",
        )
        .bind(("vid", vid.clone())).bind(("pid", page_id.clone()))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    let page = page_resp.take::<Vec<PageRow>>(0).unwrap_or_default()
        .into_iter().next()
        .ok_or_else(|| AppError::Import(format!("Page not found: {}", page_id)))?;

    let mut sess_resp = db
        .query(
            "SELECT session_id, seed_url, site_name, root_folder, status, created_at \
             FROM import_sessions WHERE vault_id = $vid AND session_id = $sid LIMIT 1",
        )
        .bind(("vid", vid.clone())).bind(("sid", session_id.clone()))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    let session = sess_resp.take::<Vec<SessionRow>>(0).unwrap_or_default()
        .into_iter().next()
        .ok_or_else(|| AppError::Import(format!("Session not found: {}", session_id)))?;

    // Import without embedding so the command returns fast (~1-2s instead of 1+ min).
    // Embedding is spawned as a background task and doesn't block the response.
    let result = import_page_inner(db, &vault_id, &page, &session.root_folder, None).await?;

    // Spawn background embedding for the just-imported note
    let emb_url: Option<String> = {
        let port = *state.embedding_actual_port.lock().await;
        port.map(|p| format!("http://127.0.0.1:{}", p))
    };
    if let Some(url) = emb_url {
        let db2 = state.db.clone();
        let note_path = result.note_path.clone();
        let vault_id2 = vault_id.clone();
        let page_id2 = page_id.clone();
        tokio::spawn(async move {
            // Read content_md from import_pages and embed its chunks
            #[derive(serde::Deserialize)]
            struct ContentRow { content_md: Option<String> }
            if let Ok(mut r) = db2.query(
                "SELECT content_md FROM import_pages WHERE vault_id = $vid AND page_id = $pid LIMIT 1"
            )
                .bind(("vid", vault_id2.clone()))
                .bind(("pid", page_id2))
                .await
            {
                let rows: Vec<ContentRow> = r.take(0).unwrap_or_default();
                if let Some(Some(content)) = rows.into_iter().next().map(|r| r.content_md) {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    let chunks = crate::vault::chunker::chunk_note(&note_path, &content, now_ms);
                    let _ = crate::vault::chunker::upsert_chunks(&db2, &vault_id2, &chunks, Some(&url)).await;
                }
            }
        });
    }

    Ok(result)
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

/// Search already-imported pages relevant to `question`.
/// Strategy: FTS → title CONTAINS fallback → all imported pages for session.
async fn find_relevant_imported_pages(
    db: &SurrealDb,
    vault_id: &str,
    session_id: Option<&str>,
    question: &str,
) -> Vec<PageItem> {
    #[derive(serde::Deserialize)]
    struct PRow { url: String, title: String, content_md: Option<String>, #[allow(dead_code)] last_crawled: Option<surrealdb::sql::Datetime> }

    let all_imported: Vec<PRow> = if let Some(sid) = session_id {
        db.query(
            "SELECT url, title, content_md, last_crawled FROM import_pages \
             WHERE vault_id = $vid AND session_id = $sid AND status = 'imported' \
             ORDER BY last_crawled DESC LIMIT 20",
        )
        .bind(("vid", vault_id.to_owned())).bind(("sid", sid.to_owned()))
        .await.ok().and_then(|mut r| r.take::<Vec<PRow>>(0).ok()).unwrap_or_default()
    } else {
        db.query(
            "SELECT url, title, content_md, last_crawled FROM import_pages \
             WHERE vault_id = $vid AND status = 'imported' \
             ORDER BY last_crawled DESC LIMIT 20",
        )
        .bind(("vid", vault_id.to_owned()))
        .await.ok().and_then(|mut r| r.take::<Vec<PRow>>(0).ok()).unwrap_or_default()
    };

    if all_imported.is_empty() {
        return vec![];
    }

    // ── Try FTS first (may fail silently on some DB versions) ────────────────
    let fts_results: Vec<PRow> = if let Some(sid) = session_id {
        db.query(
            "SELECT url, title, content_md FROM import_pages \
             WHERE vault_id = $vid AND session_id = $sid AND status = 'imported' \
             AND (title @1@ $q OR content_md @2@ $q) \
             ORDER BY search::score(1) + search::score(2) DESC LIMIT 6",
        )
        .bind(("vid", vault_id.to_owned())).bind(("sid", sid.to_owned()))
        .bind(("q", question.to_owned()))
        .await.ok().and_then(|mut r| r.take::<Vec<PRow>>(0).ok()).unwrap_or_default()
    } else {
        db.query(
            "SELECT url, title, content_md FROM import_pages \
             WHERE vault_id = $vid AND status = 'imported' \
             AND (title @1@ $q OR content_md @2@ $q) \
             ORDER BY search::score(1) + search::score(2) DESC LIMIT 6",
        )
        .bind(("vid", vault_id.to_owned())).bind(("q", question.to_owned()))
        .await.ok().and_then(|mut r| r.take::<Vec<PRow>>(0).ok()).unwrap_or_default()
    };
    if !fts_results.is_empty() {
        return fts_results.into_iter()
            .filter_map(|p| p.content_md.map(|c| PageItem { url: p.url, title: p.title, content: c }))
            .collect();
    }

    // ── Fallback: keyword scoring in Rust over all_imported ─────────────────
    let q_lower = question.to_lowercase();
    let keywords: Vec<&str> = q_lower
        .split(|c: char| !c.is_alphanumeric() && !(('\u{4E00}'..='\u{9FFF}').contains(&c)))
        .filter(|w| w.len() >= 1)
        .collect();

    let mut scored: Vec<(usize, PRow)> = all_imported.into_iter()
        .filter(|p| p.content_md.is_some())
        .map(|p| {
            let title_lower = p.title.to_lowercase();
            let content_lower = p.content_md.as_deref().unwrap_or("").to_lowercase();
            let score = keywords.iter().filter(|kw| {
                title_lower.contains(*kw) || content_lower.contains(*kw)
            }).count();
            (score, p)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));

    // Return top 4 with content, or if no keyword match just top 4 by recency
    let results: Vec<PageItem> = scored.into_iter()
        .take(4)
        .filter_map(|(_, p)| p.content_md.map(|c| PageItem { url: p.url, title: p.title, content: c }))
        .collect();
    results
}


/// Find pending pages whose sitemap title is most relevant to `question`.
/// Uses pre-embedded sitemap_titles (populated during fetch_site_outline).
/// Falls back to FTS on title if embeddings are unavailable.
async fn find_matching_pending_pages(
    db: &SurrealDb,
    vault_id: &str,
    session_id: Option<&str>,
    question: &str,
    emb_url: Option<&str>,
) -> Vec<PageRow> {
    // ── Vector search on pre-embedded sitemap_titles ──────────────────────────
    if let Some(base_url) = emb_url {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let query_vec = crate::commands::ai::get_embedding(&client, base_url, question).await;
        if !query_vec.is_empty() {
            #[derive(serde::Deserialize)]
            struct SitemapHit { url: String }
            let hits: Vec<SitemapHit> = if let Some(sid) = session_id {
                db.query(
                    "SELECT url, vector::similarity::cosine(embedding, $vec) AS score \
                     FROM sitemap_titles \
                     WHERE vault_id = $vid AND session_id = $sid AND embedding IS NOT NONE \
                     ORDER BY score DESC LIMIT 10",
                )
                .bind(("vid", vault_id.to_owned())).bind(("sid", sid.to_owned()))
                .bind(("vec", query_vec))
                .await.ok().and_then(|mut r| r.take::<Vec<SitemapHit>>(0).ok()).unwrap_or_default()
            } else {
                db.query(
                    "SELECT url, vector::similarity::cosine(embedding, $vec) AS score \
                     FROM sitemap_titles WHERE vault_id = $vid AND embedding IS NOT NONE \
                     ORDER BY score DESC LIMIT 10",
                )
                .bind(("vid", vault_id.to_owned())).bind(("vec", query_vec))
                .await.ok().and_then(|mut r| r.take::<Vec<SitemapHit>>(0).ok()).unwrap_or_default()
            };

            if !hits.is_empty() {
                let top_urls: Vec<String> = hits.into_iter().take(3).map(|h| h.url).collect();
                let pages: Vec<PageRow> = if let Some(sid) = session_id {
                    db.query(
                        "SELECT page_id, session_id, url, title, parent_url, depth, \
                         note_path, content_hash, http_etag, status, last_crawled \
                         FROM import_pages \
                         WHERE vault_id = $vid AND session_id = $sid \
                         AND status = 'pending' AND url IN $urls",
                    )
                    .bind(("vid", vault_id.to_owned())).bind(("sid", sid.to_owned()))
                    .bind(("urls", top_urls))
                    .await.ok().and_then(|mut r| r.take::<Vec<PageRow>>(0).ok()).unwrap_or_default()
                } else {
                    db.query(
                        "SELECT page_id, session_id, url, title, parent_url, depth, \
                         note_path, content_hash, http_etag, status, last_crawled \
                         FROM import_pages WHERE vault_id = $vid AND status = 'pending' AND url IN $urls",
                    )
                    .bind(("vid", vault_id.to_owned())).bind(("urls", top_urls))
                    .await.ok().and_then(|mut r| r.take::<Vec<PageRow>>(0).ok()).unwrap_or_default()
                };
                if !pages.is_empty() { return pages; }
            }
        }
    }

    // ── Fallback: FTS on sitemap_titles ───────────────────────────────────────
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct SitemapFts { url: String }
    let _fts_hits: Vec<SitemapFts> = if let Some(sid) = session_id {
        db.query(
            "SELECT url FROM sitemap_titles \
             WHERE vault_id = $vid AND session_id = $sid AND title @1@ $q \
             ORDER BY search::score(1) DESC LIMIT 3",
        )
        .bind(("vid", vault_id.to_owned())).bind(("sid", sid.to_owned()))
        .bind(("q", question.to_owned()))
        .await.ok().and_then(|mut r| r.take::<Vec<SitemapFts>>(0).ok()).unwrap_or_default()
    } else {
        db.query(
            "SELECT url FROM sitemap_titles \
             WHERE vault_id = $vid AND title @1@ $q ORDER BY search::score(1) DESC LIMIT 3",
        )
        .bind(("vid", vault_id.to_owned())).bind(("q", question.to_owned()))
        .await.ok().and_then(|mut r| r.take::<Vec<SitemapFts>>(0).ok()).unwrap_or_default()
    };

    // ── Final fallback: keyword match on import_pages.title in Rust ─────────
    // (covers both pending pages and works without FTS or sitemap_titles)
    let all_pending: Vec<PageRow> = if let Some(sid) = session_id {
        db.query(
            "SELECT page_id, session_id, url, title, parent_url, depth, \
             note_path, content_hash, http_etag, status, last_crawled \
             FROM import_pages WHERE vault_id = $vid AND session_id = $sid \
             AND status = 'pending' LIMIT 100",
        )
        .bind(("vid", vault_id.to_owned())).bind(("sid", sid.to_owned()))
        .await.ok().and_then(|mut r| r.take::<Vec<PageRow>>(0).ok()).unwrap_or_default()
    } else {
        db.query(
            "SELECT page_id, session_id, url, title, parent_url, depth, \
             note_path, content_hash, http_etag, status, last_crawled \
             FROM import_pages WHERE vault_id = $vid AND status = 'pending' LIMIT 100",
        )
        .bind(("vid", vault_id.to_owned()))
        .await.ok().and_then(|mut r| r.take::<Vec<PageRow>>(0).ok()).unwrap_or_default()
    };

    if all_pending.is_empty() { return vec![]; }

    let q_lower = question.to_lowercase();
    let keywords: Vec<&str> = q_lower
        .split(|c: char| !c.is_alphanumeric() && !(('\u{4E00}'..='\u{9FFF}').contains(&c)))
        .filter(|w| w.len() >= 1)
        .collect();
    if keywords.is_empty() { return vec![]; }

    let mut scored: Vec<(usize, PageRow)> = all_pending.into_iter().map(|page| {
        let title_lower = page.title.to_lowercase();
        let score = keywords.iter().filter(|kw| title_lower.contains(*kw)).count();
        (score, page)
    }).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().filter(|(s, _)| *s > 0).take(3).map(|(_, p)| p).collect()
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
    // ── Get embedding URL (only used for sitemap title vector search) ────────
    let emb_url: Option<String> = {
        let port = *app_state.embedding_actual_port.lock().await;
        port.map(|p| format!("http://127.0.0.1:{}", p))
    };

    // ── 1. FTS search on already-imported pages ────────────────────────────────
    let mut notes = find_relevant_imported_pages(db, vault_id, session_id, question).await;

    // ── 2. On-demand import of matching pending pages if no content found ──────
    if notes.is_empty() {
        let pending = find_matching_pending_pages(db, vault_id, session_id, question, emb_url.as_deref()).await;
        if !pending.is_empty() {
            let titles: Vec<&str> = pending.iter().map(|p| p.title.as_str()).collect();
            let _ = app.emit("knowledge:importing_pages", serde_json::json!({
                "query_id": query_id,
                "titles": &titles,
            }));

            // Fetch all pending pages sequentially to avoid concurrent write issues
            let mut import_errors: Vec<String> = Vec::new();
            for page in &pending {
                if let Err(e) = fetch_page_content_for_qa(db, vault_id, page).await {
                    import_errors.push(format!("{}: {}", page.title, e));
                }
            }
            if !import_errors.is_empty() {
                log::warn!("[knowledge] on-demand import errors: {:?}", import_errors);
            }

            // Re-search with newly imported content
            notes = find_relevant_imported_pages(db, vault_id, session_id, question).await;

            // Debug: if still empty, emit diagnostic info
            if notes.is_empty() {
                #[derive(serde::Deserialize)]
                struct DebugRow { page_id: String, status: String, title: String, has_content: bool }
                let debug_rows: Vec<DebugRow> = db.query(
                    "SELECT page_id, status, title, content_md != NONE AS has_content \
                     FROM import_pages WHERE vault_id = $vid AND session_id = $sid LIMIT 10"
                )
                .bind(("vid", vault_id.to_owned()))
                .bind(("sid", session_id.unwrap_or("").to_owned()))
                .await.ok()
                .and_then(|mut r| r.take::<Vec<DebugRow>>(0).ok())
                .unwrap_or_default();
                let debug_info: Vec<String> = debug_rows.iter()
                    .map(|r| format!("[{}] status={} has_content={} title={}", r.page_id, r.status, r.has_content, r.title))
                    .collect();
                log::warn!("[knowledge] still empty after import. pages for session: {:?}. import_errors: {:?}", debug_info, import_errors);
                let _ = app.emit("knowledge:debug", serde_json::json!({
                    "query_id": query_id,
                    "pages": debug_info,
                    "import_errors": import_errors,
                }));
            }
        }
    }

    if notes.is_empty() {
        let msg = "目前沒有相關的知識點。請嘗試重新表述問題，或到「管理來源」手動匯入更多頁面。";
        let _ = app.emit("knowledge:token", serde_json::json!({ "query_id": query_id, "content": msg }));
        let _ = app.emit("knowledge:done", serde_json::json!({ "query_id": query_id }));
        return Ok(());
    }

    // 2. Build refs (emitted immediately so UI can show sources while LLM streams)
    let refs: Vec<KnowledgeRef> = notes.iter().map(|n| {
        let title = if n.title.is_empty() {
            n.url.clone()
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
        KnowledgeRef { path: n.url.clone(), title, excerpt }
    }).collect();

    let _ = app.emit("knowledge:refs", serde_json::json!({ "query_id": query_id, "refs": refs }));

    // 3. Build RAG context
    let is_cross_note = {
        let q = question.to_lowercase();
        q.contains("比較") || q.contains("對比") || q.contains("異同") || q.contains("差異")
        || q.contains("總結") || q.contains("綜合") || q.contains("差別") || q.contains("共同")
        || q.contains("相同") || q.contains("不同") || q.contains("compare") || q.contains("synthesize")
    };
    if is_cross_note {
        let _ = app.emit("knowledge:cross_note", serde_json::json!({ "query_id": query_id }));
    }

    let context = notes.iter().enumerate().map(|(i, n)| {
        let body = if n.content.starts_with("---") {
            n.content.splitn(4, "---").nth(2).unwrap_or(&n.content).trim_start().to_string()
        } else {
            n.content.clone()
        };
        let excerpt: String = body.chars().take(1200).collect();
        format!("[{}] 標題：{}\n{}", i + 1, n.title, excerpt)
    }).collect::<Vec<_>>().join("\n\n---\n\n");

    let system = if is_cross_note {
        format!(
            "你是知識庫跨筆記推理助手。根據以下多篇筆記進行比較、對比或綜合分析。\
            如有引用，以 [1][2] 格式標示來源編號。\
            若有多個來源可以比較，請用結構化方式（如對比清單）呈現。\
            若筆記內容不足，誠實說明。用繁體中文回答。\n\n筆記：\n\n{}",
            context
        )
    } else {
        format!(
            "你是知識庫問答助手。根據以下筆記回答使用者問題。\
            如有引用，以 [1][2] 格式標示來源編號。\
            若筆記內容不足，誠實說明。用繁體中文回答。\n\n筆記：\n\n{}",
            context
        )
    };

    // 5. Read AI provider config
    let provider = queries::get_setting(db, "ai_provider")
        .await.unwrap_or_default().unwrap_or_default();
    let model = queries::get_setting(db, "ai_model")
        .await.unwrap_or_default().unwrap_or_default();
    let api_key = read_api_key(&app_state.api_key_cache, &provider).await;

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
    pub auto_tool_calls: Vec<String>, // 自動呼叫的工具，限 search_vault/read_note/list_structure
    #[serde(default = "default_passive")]
    pub injection_mode: String,   // "passive"（embedding 比對）或 "active"（永遠注入）
    #[serde(default = "default_scope_all")]
    pub agent_scope: String,      // "all" | "main" | "search" | "write" | "research" | "memory"
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
    pub auto_tool_calls: Vec<String>,
    pub is_active: bool,
    pub injection_mode: String,   // "passive" | "active"
    pub agent_scope: String,      // "all" | "main" | "search" | "write" | "research" | "memory"
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
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;

    // 取得頁面 content_md（DB 儲存，不讀磁碟）
    #[derive(Deserialize)]
    struct PageContent { content_md: Option<String> }
    let mut r = db.query(
        "SELECT content_md FROM import_pages WHERE vault_id = $vid AND page_id = $pid LIMIT 1"
    ).bind(("vid", vault_id.clone())).bind(("pid", page_id.clone()))
    .await.map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<PageContent> = r.take(0).unwrap_or_default();
    let content = rows.into_iter().next()
        .and_then(|p| p.content_md)
        .ok_or_else(|| AppError::Import("頁面尚未匯入或內容不存在".to_string()))?;

    // 截取前 3000 字元，避免 context 過長
    let excerpt = if content.len() > 3000 { &content[..3000] } else { &content };

    let system_prompt = r#"你是一個知識管理專家。根據用戶提供的文章，建議 2-4 個值得建立的知識卡片。
每張卡片必須是以下三種類型之一：
- concept（概念定義）：適合解釋一個術語、概念或原理
- procedure（操作步驟）：適合記錄一個操作流程或步驟
- reference（參考資料）：適合整理一個主題的參考摘要

回傳嚴格的 JSON 陣列格式（不要有任何其他文字）：
[
  {
    "title": "卡片標題",
    "template": "concept | procedure | reference",
    "content": "完整的 markdown 內容（含 frontmatter）",
    "reason": "為什麼這個知識值得建立成獨立卡片"
  }
]

content 欄位格式範例（concept）：
---
status: draft
tags: [concept]
---

# 標題

## 定義

> 簡短定義

## 詳細說明

（從文章中提取的核心內容）

## 相關概念

-

## 來源

- 原文標題"#;

    let user_content = format!("請根據以下文章建議知識卡片：\n\n{}", excerpt);

    // 呼叫 LLM
    let base_url = ensure_server_running(state.inner(), &app).await?;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user",   "content": user_content},
        ],
        "max_tokens": 2048,
        "temperature": 0.3,
        "stream": false,
    });

    let response = client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| AppError::AI(format!("LLM 請求失敗：{}", e)))?;

    let json: serde_json::Value = response.json().await
        .map_err(|e| AppError::AI(format!("回應解析失敗：{}", e)))?;

    let raw = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    // 嘗試解析 JSON（LLM 可能在前後加說明文字，找 [ ... ]）
    let json_start = raw.find('[').unwrap_or(0);
    let json_end = raw.rfind(']').map(|i| i + 1).unwrap_or(raw.len());
    let json_str = &raw[json_start..json_end];

    let suggestions: Vec<KBCardSuggestion> = serde_json::from_str(json_str)
        .unwrap_or_default();

    // 存入 DB（先刪同 page 舊建議，再逐筆 insert）
    if !suggestions.is_empty() {
        let _ = state.db.query(
            "DELETE FROM kb_suggestions WHERE vault_id = $vid AND page_id = $pid"
        )
        .bind(("vid", vault_id.clone()))
        .bind(("pid", page_id.clone()))
        .await;

        let now_ms = chrono::Local::now().timestamp_millis();
        for s in &suggestions {
            let sid = uuid::Uuid::new_v4().to_string();
            let _ = state.db.query(
                "INSERT INTO kb_suggestions (suggestion_id, vault_id, session_id, page_id, title, template, content, reason, created_at) \
                 VALUES ($sid, $vid, $sess, $pid, $title, $tmpl, $content, $reason, $now)"
            )
            .bind(("sid", sid))
            .bind(("vid", vault_id.clone()))
            .bind(("sess", session_id.clone()))
            .bind(("pid", page_id.clone()))
            .bind(("title", s.title.clone()))
            .bind(("tmpl", s.template.clone()))
            .bind(("content", s.content.clone()))
            .bind(("reason", s.reason.clone()))
            .bind(("now", now_ms))
            .await;
        }
    }

    Ok(suggestions)
}

/// 載入已存入 DB 的知識卡片建議（按 session 或 page 過濾）
#[tauri::command]
pub async fn list_kb_suggestions(
    state: State<'_, AppState>,
    session_id: Option<String>,
    page_id: Option<String>,
) -> Result<Vec<KBSuggestionRecord>, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;

    #[derive(serde::Deserialize)]
    struct Row {
        suggestion_id: String,
        session_id: String,
        page_id: String,
        title: String,
        template: String,
        content: String,
        reason: String,
        created_at: i64,
    }

    let rows: Vec<Row> = if let Some(pid) = page_id {
        let mut r = db.query(
            "SELECT suggestion_id, session_id, page_id, title, template, content, reason, created_at \
             FROM kb_suggestions WHERE vault_id = $vid AND page_id = $pid ORDER BY created_at ASC"
        ).bind(("vid", vault_id)).bind(("pid", pid))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
        r.take(0).unwrap_or_default()
    } else if let Some(sid) = session_id {
        let mut r = db.query(
            "SELECT suggestion_id, session_id, page_id, title, template, content, reason, created_at \
             FROM kb_suggestions WHERE vault_id = $vid AND session_id = $sid ORDER BY created_at ASC"
        ).bind(("vid", vault_id)).bind(("sid", sid))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
        r.take(0).unwrap_or_default()
    } else {
        let mut r = db.query(
            "SELECT suggestion_id, session_id, page_id, title, template, content, reason, created_at \
             FROM kb_suggestions WHERE vault_id = $vid ORDER BY created_at ASC"
        ).bind(("vid", vault_id))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
        r.take(0).unwrap_or_default()
    };

    Ok(rows.into_iter().map(|r| KBSuggestionRecord {
        suggestion_id: r.suggestion_id,
        session_id: r.session_id,
        page_id: r.page_id,
        title: r.title,
        template: r.template,
        content: r.content,
        reason: r.reason,
        created_at: r.created_at,
    }).collect())
}

/// 刪除單筆建議
#[tauri::command]
pub async fn dismiss_kb_suggestion(
    state: State<'_, AppState>,
    suggestion_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    state.db.query(
        "DELETE FROM kb_suggestions WHERE vault_id = $vid AND suggestion_id = $sid"
    )
    .bind(("vid", vault_id))
    .bind(("sid", suggestion_id))
    .await.map_err(|e| AppError::Database(e.to_string()))?;
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
    suggestion_id: String,
    session_id: String,
    title: String,
    content: String,
) -> Result<String, AppError> {
    let vault_id = state.get_vault_id().await?;
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }

    // Query all notes already created from this session's AI cards
    #[derive(serde::Deserialize)]
    struct SiblingRow { title: String, created_note_path: Option<String> }
    let mut resp = state.db.query(
        "SELECT title, created_note_path FROM kb_suggestions \
         WHERE vault_id = $vid AND session_id = $sess \
         AND created_note_path != NONE AND suggestion_id != $me"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("sess", session_id.clone()))
    .bind(("me", suggestion_id.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    let siblings: Vec<SiblingRow> = resp.take(0).unwrap_or_default();

    let sibling_titles: Vec<(String, String)> = siblings.into_iter()
        .filter_map(|s| s.created_note_path.map(|p| (s.title, p)))
        .collect();

    // Inject wikilinks into content
    let final_content = inject_wikilinks_to_siblings(&content, &sibling_titles);

    // Create vault note (mirrors create_note logic)
    let safe_title: String = title.chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    let filename = format!("{}.md", safe_title.trim());
    let rel_path = filename.clone();

    let abs_path = std::path::PathBuf::from(&vault_path).join(&rel_path);
    tokio::fs::write(&abs_path, final_content.as_bytes()).await?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let now_dt = surrealdb::sql::Datetime::from(
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms).unwrap_or_default()
    );
    let checksum = {
        let hash = Sha256::digest(final_content.as_bytes());
        format!("{:x}", hash)
    };
    let word_count = final_content.split_whitespace().count() as i64;

    state.db.query(
        "INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at, checksum)
         VALUES ($vid, $path, $title, $content, $wc, $now, $now, $cs)"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("path", rel_path.clone()))
    .bind(("title", title.clone()))
    .bind(("content", final_content.clone()))
    .bind(("wc", word_count))
    .bind(("now", now_dt))
    .bind(("cs", checksum))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    // Build chunks + embeddings
    {
        let chunks = crate::vault::chunker::chunk_note(&rel_path, &final_content, now_ms);
        let emb_url: Option<String> = {
            let port = *state.embedding_actual_port.lock().await;
            port.map(|p| format!("http://127.0.0.1:{}", p))
        };
        let _ = crate::vault::chunker::upsert_chunks(&state.db, &vault_id, &chunks, emb_url.as_deref()).await;
    }

    // Mark suggestion as created (store note_path) and remove from suggestions list
    let _ = state.db.query(
        "UPDATE kb_suggestions SET created_note_path = $path \
         WHERE vault_id = $vid AND suggestion_id = $sid"
    )
    .bind(("path", rel_path.clone()))
    .bind(("vid", vault_id.clone()))
    .bind(("sid", suggestion_id.clone()))
    .await;

    // Also backfill wikilinks in already-created sibling notes that mention this new title
    // (only update the vault file and notes DB — lightweight, non-blocking)
    let new_title = title.clone();
    let new_wikilink = format!("[[{}]]", new_title);
    for (sib_title, sib_path) in &sibling_titles {
        let _ = sib_title; // used via already-created path
        let abs_sib = std::path::PathBuf::from(&vault_path).join(sib_path);
        if let Ok(sib_content) = tokio::fs::read_to_string(&abs_sib).await {
            // Only update if the new title appears in sibling but isn't already wikilinked
            if sib_content.contains(new_title.as_str()) && !sib_content.contains(&new_wikilink) {
                let updated = inject_wikilinks_to_siblings(&sib_content, &[(new_title.clone(), rel_path.clone())]);
                let _ = tokio::fs::write(&abs_sib, &updated).await;
                let _ = state.db.query(
                    "UPDATE notes SET content = $c WHERE vault_id = $vid AND path = $p"
                )
                .bind(("c", updated))
                .bind(("vid", vault_id.clone()))
                .bind(("p", sib_path.to_owned()))
                .await;
            }
        }
    }

    Ok(rel_path)
}

/// 搜尋已驗證 KB chunks，回傳格式化的 context 字串供注入 system prompt。
/// 依序嘗試：向量 → BM25 → contains（僅 verified）。
/// 找到結果時回傳 Some(context)，找不到回傳 None。
pub async fn search_kb_context(
    db: &SurrealDb,
    vault_id: &str,
    query: &str,
    embedding_url: Option<&str>,
) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct ChunkRow { file_path: String, section: String, content: String }

    let client = reqwest::Client::new();

    // 1. Vector
    let chunks: Vec<ChunkRow> = if let Some(url) = embedding_url {
        let qvec = get_embedding(&client, url, query).await;
        if !qvec.is_empty() {
            let mut r = db.query(
                "SELECT file_path, section, content,
                        vector::similarity::cosine(embedding, $qvec) AS score
                 FROM chunks
                 WHERE vault_id = $vid AND embedding != NONE AND status = 'verified'
                 ORDER BY score DESC LIMIT 5"
            ).bind(("vid", vault_id.to_owned())).bind(("qvec", qvec))
            .await.ok()?;
            r.take(0).unwrap_or_default()
        } else { vec![] }
    } else { vec![] };

    // 2. BM25
    let chunks: Vec<ChunkRow> = if chunks.is_empty() {
        let mut r = db.query(
            "SELECT file_path, section, content FROM chunks
             WHERE vault_id = $vid AND status = 'verified' AND content @1@ $q LIMIT 5"
        ).bind(("vid", vault_id.to_owned())).bind(("q", query.to_owned()))
        .await.ok()?;
        r.take(0).unwrap_or_default()
    } else { chunks };

    // 3. Contains
    let chunks: Vec<ChunkRow> = if chunks.is_empty() {
        let mut r = db.query(
            "SELECT file_path, section, content FROM chunks
             WHERE vault_id = $vid AND status = 'verified'
               AND string::contains(string::lowercase(content), string::lowercase($q))
             LIMIT 5"
        ).bind(("vid", vault_id.to_owned())).bind(("q", query.to_owned()))
        .await.ok()?;
        r.take(0).unwrap_or_default()
    } else { chunks };

    if chunks.is_empty() { return None; }

    let mut context = String::from(
        "## 知識庫參考資料（已驗證）\n\
         以下內容來自你的知識庫，請優先參考，引用時標注 [KB: 來源]：\n\n"
    );
    for (i, c) in chunks.iter().enumerate() {
        let fname = c.file_path.split('/').last().unwrap_or("").trim_end_matches(".md");
        let source = if c.section.is_empty() {
            fname.to_string()
        } else {
            format!("{} § {}", fname, c.section)
        };
        let snippet = if c.content.len() > 400 {
            format!("{}…", &c.content[..400])
        } else {
            c.content.clone()
        };
        context.push_str(&format!("[{}] 來源：{}\n{}\n\n", i + 1, source, snippet));
    }
    context.push_str("---\n若知識庫內容足以回答請標注來源；不足時可補充，但請區分知識庫來源與你的推論。\n");
    Some(context)
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
/// Spawns a background task to chunk + embed the combined source content.
#[tauri::command]
pub async fn save_knowledge_item(
    state: State<'_, AppState>,
    session_id: String,
    title: String,
    ai_summary: String,
    source_refs: Vec<KnowledgeRef>,
) -> Result<KnowledgeItem, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let item_id = Uuid::new_v4().to_string();
    let vid = vault_id.clone();
    let sid = session_id.clone();

    db.query(
        "INSERT INTO knowledge_items (vault_id, item_id, session_id, title, source_refs, ai_summary, created_at) \
         VALUES ($vid, $iid, $sid, $title, $refs, $summary, time::now())"
    )
    .bind(("vid", vid.clone())).bind(("iid", item_id.clone()))
    .bind(("sid", sid.clone())).bind(("title", title.clone()))
    .bind(("refs", source_refs.clone())).bind(("summary", ai_summary.clone()))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    // Background: fetch source page content and chunk + embed into `chunks` table
    let emb_url: Option<String> = {
        let port = *state.embedding_actual_port.lock().await;
        port.map(|p| format!("http://127.0.0.1:{}", p))
    };
    if let Some(emb_url) = emb_url {
        let db2 = state.db.clone();
        let vid2 = vault_id.clone();
        let iid = item_id.clone();
        let summary = ai_summary.clone();
        let ref_urls: Vec<String> = source_refs.iter().map(|r| r.path.clone()).collect();
        tokio::spawn(async move {
            #[derive(serde::Deserialize)]
            struct ContentRow { title: String, content_md: Option<String> }
            // Fetch all referenced page contents
            let mut combined = format!("# AI 整理摘要\n\n{}\n\n", summary);
            for url in &ref_urls {
                if let Ok(mut r) = db2.query(
                    "SELECT title, content_md FROM import_pages \
                     WHERE vault_id = $vid AND url = $url LIMIT 1"
                )
                .bind(("vid", vid2.clone())).bind(("url", url.clone()))
                .await {
                    let rows: Vec<ContentRow> = r.take(0).unwrap_or_default();
                    if let Some(row) = rows.into_iter().next() {
                        if let Some(md) = row.content_md {
                            combined.push_str(&format!("## {}\n\n{}\n\n", row.title, md));
                        }
                    }
                }
            }
            let file_path = format!("knowledge_items/{}.md", iid);
            let now_ms = chrono::Utc::now().timestamp_millis();
            let chunks = crate::vault::chunker::chunk_note(&file_path, &combined, now_ms);
            // Store chunks with item_id reference
            for chunk in &chunks {
                let _ = db2.query(
                    "INSERT INTO chunks \
                     (vault_id, chunk_id, file_path, section, content, links, chunk_type, word_count, updated_at, item_id) \
                     VALUES ($vid, $cid, $fp, $section, $content, $links, $chunk_type, $wc, time::now(), $iid) \
                     ON DUPLICATE KEY UPDATE content = $content, item_id = $iid, updated_at = time::now()"
                )
                .bind(("vid", vid2.clone())).bind(("cid", chunk.id.clone()))
                .bind(("fp", chunk.file_path.clone())).bind(("section", chunk.section.clone()))
                .bind(("content", chunk.content.clone())).bind(("links", chunk.links.clone()))
                .bind(("chunk_type", chunk.chunk_type.clone())).bind(("wc", chunk.word_count))
                .bind(("iid", iid.clone()))
                .await.ok();
            }
            // Now embed them using the chunker (re-upsert with embedding)
            let _ = crate::vault::chunker::upsert_chunks(&db2, &vid2, &chunks, Some(&emb_url)).await;
        });
    }

    let created_at = chrono::Utc::now().timestamp_millis();
    Ok(KnowledgeItem {
        item_id,
        vault_id,
        session_id,
        title,
        source_refs,
        ai_summary,
        created_at,
    })
}

/// List all knowledge items for the current vault, newest first.
#[tauri::command]
pub async fn list_knowledge_items(
    state: State<'_, AppState>,
) -> Result<Vec<KnowledgeItem>, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;

    #[derive(Deserialize)]
    struct KIRow {
        item_id: String,
        vault_id: String,
        session_id: String,
        title: String,
        source_refs: Vec<KnowledgeRef>,
        ai_summary: String,
        created_at: surrealdb::sql::Datetime,
    }
    let mut resp = db.query(
        "SELECT item_id, vault_id, session_id, title, source_refs, ai_summary, created_at \
         FROM knowledge_items WHERE vault_id = $vid ORDER BY created_at DESC"
    )
    .bind(("vid", vault_id))
    .await.map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<KIRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    Ok(rows.into_iter().map(|r| {
        let created_at = r.created_at.timestamp_millis();
        KnowledgeItem {
            item_id: r.item_id, vault_id: r.vault_id, session_id: r.session_id,
            title: r.title, source_refs: r.source_refs, ai_summary: r.ai_summary, created_at,
        }
    }).collect())
}

/// Get a single knowledge item by item_id.
#[tauri::command]
pub async fn get_knowledge_item(
    state: State<'_, AppState>,
    item_id: String,
) -> Result<KnowledgeItem, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;

    #[derive(Deserialize)]
    struct KIRow {
        item_id: String,
        vault_id: String,
        session_id: String,
        title: String,
        source_refs: Option<String>,
        ai_summary: String,
        created_at: surrealdb::sql::Datetime,
    }
    let mut resp = db.query(
        "SELECT item_id, vault_id, session_id, title, source_refs, ai_summary, created_at \
         FROM knowledge_items WHERE vault_id = $vid AND item_id = $iid LIMIT 1"
    )
    .bind(("vid", vault_id)).bind(("iid", item_id.clone()))
    .await.map_err(|e| AppError::Database(e.to_string()))?;
    let row = resp.take::<Vec<KIRow>>(0).map_err(|e| AppError::Database(e.to_string()))?
        .into_iter().next()
        .ok_or_else(|| AppError::Import(format!("knowledge item not found: {}", item_id)))?;

    let source_refs: Vec<KnowledgeRef> = row.source_refs
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Ok(KnowledgeItem {
        item_id: row.item_id, vault_id: row.vault_id, session_id: row.session_id,
        title: row.title, source_refs, ai_summary: row.ai_summary,
        created_at: row.created_at.timestamp_millis(),
    })
}

/// Delete a knowledge item and its chunks.
#[tauri::command]
pub async fn delete_knowledge_item(
    state: State<'_, AppState>,
    item_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let vid = vault_id.clone();
    let iid = item_id.clone();
    db.query("DELETE FROM knowledge_items WHERE vault_id = $vid AND item_id = $iid")
        .bind(("vid", vid.clone())).bind(("iid", iid.clone()))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    db.query("DELETE FROM chunks WHERE vault_id = $vid AND item_id = $iid")
        .bind(("vid", vid)).bind(("iid", iid))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// Rename a knowledge item title.
#[tauri::command]
pub async fn rename_knowledge_item(
    state: State<'_, AppState>,
    item_id: String,
    title: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    db.query("UPDATE knowledge_items SET title = $title WHERE vault_id = $vid AND item_id = $iid")
        .bind(("title", title))
        .bind(("vid", vault_id))
        .bind(("iid", item_id))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

// ── Agent Skills CRUD ─────────────────────────────────────────────────────────

/// 儲存技能規範至 agent_skills，並同步計算 trigger embedding（供向量搜尋）。
#[tauri::command]
pub async fn save_agent_skill(
    app: AppHandle,
    state: State<'_, AppState>,
    knowledge_item_id: String,
    title: String,
    trigger: String,
    behavior: String,
    auto_tool_calls: Vec<String>,
    injection_mode: Option<String>,
    agent_scope: Option<String>,
) -> Result<AgentSkillRecord, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let skill_id = uuid::Uuid::new_v4().to_string();
    let mode = injection_mode.unwrap_or_else(|| "passive".to_string());
    let mode = if mode == "active" { "active" } else { "passive" };
    let scope = valid_scope(agent_scope.as_deref().unwrap_or("all")).to_string();

    // 計算 trigger 向量（若 embedding server 未就緒則存 None）
    let emb_url: Option<String> = {
        let port = *state.embedding_actual_port.lock().await;
        port.map(|p| format!("http://127.0.0.1:{}", p))
    };
    let trigger_embedding: Option<Vec<f32>> = if let Some(ref url) = emb_url {
        let base_url = crate::commands::ai::ensure_server_running(state.inner(), &app).await.ok();
        if base_url.is_some() {
            let client = reqwest::Client::new();
            let vec = crate::commands::ai::get_embedding(&client, url, &trigger).await;
            if vec.is_empty() { None } else { Some(vec) }
        } else { None }
    } else { None };

    // 篩選合法工具（防止任意工具被注入）
    let allowed = ["search_vault", "read_note", "list_structure"];
    let safe_tools: Vec<String> = auto_tool_calls.into_iter()
        .filter(|t| allowed.contains(&t.as_str()))
        .collect();

    db.query(
        "INSERT INTO agent_skills \
         (skill_id, vault_id, knowledge_item_id, title, trigger, behavior, \
          auto_tool_calls, is_active, injection_mode, agent_scope, trigger_count, trigger_embedding, created_at) \
         VALUES ($sid, $vid, $kid, $title, $trigger, $behavior, \
                 $tools, true, $mode, $scope, 0, $emb, time::now())"
    )
    .bind(("sid", skill_id.clone()))
    .bind(("vid", vault_id.clone()))
    .bind(("kid", knowledge_item_id.clone()))
    .bind(("title", title.clone()))
    .bind(("trigger", trigger.clone()))
    .bind(("behavior", behavior.clone()))
    .bind(("tools", safe_tools.clone()))
    .bind(("mode", mode.to_string()))
    .bind(("scope", scope.clone()))
    .bind(("emb", trigger_embedding))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    let created_at = chrono::Utc::now().timestamp_millis();
    Ok(AgentSkillRecord {
        skill_id,
        vault_id,
        knowledge_item_id,
        title,
        trigger,
        behavior,
        auto_tool_calls: safe_tools,
        is_active: true,
        injection_mode: mode.to_string(),
        agent_scope: scope,
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
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;

    let since_ts = chrono::Utc::now() - chrono::Duration::days(30);
    let since_str = since_ts.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // ── 全局 30 天觸發記錄 ────────────────────────────────────────────────────
    #[derive(Deserialize)]
    struct LogRow { skill_id: String, triggered_at: surrealdb::sql::Datetime }

    let mut r = db.query(
        "SELECT skill_id, triggered_at FROM skill_usage_log \
         WHERE vault_id = $vid AND triggered_at > $since \
         ORDER BY triggered_at ASC"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("since", since_str.clone()))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    let logs: Vec<LogRow> = r.take(0).unwrap_or_default();

    // 建立 30 天日期表（確保每天都有，即使為 0）
    let today = chrono::Utc::now().date_naive();
    let dates: Vec<String> = (0..30).rev()
        .map(|d| (today - chrono::Duration::days(d)).format("%Y-%m-%d").to_string())
        .collect();

    // 全局每日聚合
    let mut global_map: std::collections::HashMap<String, i64> =
        dates.iter().map(|d| (d.clone(), 0)).collect();
    // per-skill 每日聚合
    let mut skill_map: std::collections::HashMap<String, std::collections::HashMap<String, i64>> =
        std::collections::HashMap::new();

    for log in &logs {
        let dt: chrono::DateTime<chrono::Utc> = log.triggered_at.clone().into();
        let date_str = dt.format("%Y-%m-%d").to_string();
        *global_map.entry(date_str.clone()).or_insert(0) += 1;
        skill_map.entry(log.skill_id.clone()).or_default()
            .entry(date_str).or_insert(0);
        *skill_map.entry(log.skill_id.clone()).or_default()
            .entry(dt.format("%Y-%m-%d").to_string()).or_insert(0) += 1;
    }

    let global_daily: Vec<DailyCount> = dates.iter()
        .map(|d| DailyCount { date: d.clone(), count: *global_map.get(d).unwrap_or(&0) })
        .collect();
    let total_triggers_30d: i64 = global_daily.iter().map(|d| d.count).sum();

    // ── 取 top 10 skills（by trigger_count）及 active_count ──────────────────
    #[derive(Deserialize)]
    struct SkillRow {
        skill_id: String,
        title: String,
        trigger_count: i64,
        is_active: bool,
    }
    let mut r2 = db.query(
        "SELECT skill_id, title, trigger_count, is_active FROM agent_skills \
         WHERE vault_id = $vid ORDER BY trigger_count DESC LIMIT 10"
    )
    .bind(("vid", vault_id.clone()))
    .await.map_err(|e| AppError::Database(e.to_string()))?;
    let skill_rows: Vec<SkillRow> = r2.take(0).unwrap_or_default();

    let active_count = skill_rows.iter().filter(|s| s.is_active).count() as i64;

    let top_skills: Vec<SkillTrendStat> = skill_rows.into_iter().map(|s| {
        let per_day = skill_map.get(&s.skill_id);
        let daily = dates.iter().map(|d| DailyCount {
            date: d.clone(),
            count: per_day.and_then(|m| m.get(d)).copied().unwrap_or(0),
        }).collect();
        SkillTrendStat {
            skill_id: s.skill_id,
            title: s.title,
            trigger_count: s.trigger_count,
            daily,
        }
    }).collect();

    Ok(SkillUsageStats { global_daily, top_skills, active_count, total_triggers_30d })
}

/// 更新技能規範內容並重算 trigger embedding。
#[tauri::command]
pub async fn update_agent_skill(
    app: AppHandle,
    state: State<'_, AppState>,
    skill_id: String,
    title: String,
    trigger: String,
    behavior: String,
    auto_tool_calls: Vec<String>,
    injection_mode: Option<String>,
    agent_scope: Option<String>,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;

    let mode = injection_mode.unwrap_or_else(|| "passive".to_string());
    let mode = if mode == "active" { "active" } else { "passive" };
    let scope = valid_scope(agent_scope.as_deref().unwrap_or("all")).to_string();

    let allowed = ["search_vault", "read_note", "list_structure"];
    let safe_tools: Vec<String> = auto_tool_calls.into_iter()
        .filter(|t| allowed.contains(&t.as_str()))
        .collect();

    // 重算 trigger embedding
    let emb_url: Option<String> = {
        let port = *state.embedding_actual_port.lock().await;
        port.map(|p| format!("http://127.0.0.1:{}", p))
    };
    let trigger_embedding: Option<Vec<f32>> = if let Some(ref url) = emb_url {
        let base_url = crate::commands::ai::ensure_server_running(state.inner(), &app).await.ok();
        if base_url.is_some() {
            let client = reqwest::Client::new();
            let vec = crate::commands::ai::get_embedding(&client, url, &trigger).await;
            if vec.is_empty() { None } else { Some(vec) }
        } else { None }
    } else { None };

    db.query(
        "UPDATE agent_skills SET title = $title, trigger = $trigger, behavior = $behavior, \
         auto_tool_calls = $tools, injection_mode = $mode, agent_scope = $scope, trigger_embedding = $emb \
         WHERE vault_id = $vid AND skill_id = $sid"
    )
    .bind(("title", title))
    .bind(("trigger", trigger))
    .bind(("behavior", behavior))
    .bind(("tools", safe_tools))
    .bind(("mode", mode.to_string()))
    .bind(("scope", scope))
    .bind(("emb", trigger_embedding))
    .bind(("vid", vault_id))
    .bind(("sid", skill_id))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    Ok(())
}

/// 列出 vault 中所有技能規範，可選擇只回傳特定知識項目或只回傳啟用中的技能。
#[tauri::command]
pub async fn list_agent_skills(
    state: State<'_, AppState>,
    knowledge_item_id: Option<String>,
    active_only: bool,
) -> Result<Vec<AgentSkillRecord>, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;

    #[derive(Deserialize)]
    struct SkillRow {
        skill_id: String,
        vault_id: String,
        knowledge_item_id: String,
        title: String,
        trigger: String,
        behavior: String,
        auto_tool_calls: Vec<String>,
        is_active: bool,
        #[serde(default = "default_passive")]
        injection_mode: String,
        #[serde(default = "default_scope_all")]
        agent_scope: String,
        trigger_count: i64,
        last_triggered_at: Option<surrealdb::sql::Datetime>,
        created_at: surrealdb::sql::Datetime,
    }

    let mut query = "SELECT skill_id, vault_id, knowledge_item_id, title, trigger, behavior, \
                     auto_tool_calls, is_active, injection_mode OR 'passive' AS injection_mode, \
                     agent_scope OR 'all' AS agent_scope, \
                     trigger_count, last_triggered_at, created_at \
                     FROM agent_skills WHERE vault_id = $vid".to_string();
    if active_only { query.push_str(" AND is_active = true"); }
    if knowledge_item_id.is_some() { query.push_str(" AND knowledge_item_id = $kid"); }
    query.push_str(" ORDER BY created_at DESC");

    let mut req = db.query(query).bind(("vid", vault_id.clone()));
    if let Some(ref kid) = knowledge_item_id {
        req = req.bind(("kid", kid.clone()));
    }
    let mut resp = req.await.map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<SkillRow> = resp.take(0).unwrap_or_else(|e| {
        eprintln!("[list_agent_skills] deserialize error: {e}");
        vec![]
    });

    Ok(rows.into_iter().map(|r| AgentSkillRecord {
        skill_id: r.skill_id,
        vault_id: r.vault_id,
        knowledge_item_id: r.knowledge_item_id,
        title: r.title,
        trigger: r.trigger,
        behavior: r.behavior,
        auto_tool_calls: r.auto_tool_calls,
        is_active: r.is_active,
        injection_mode: r.injection_mode,
        agent_scope: r.agent_scope,
        trigger_count: r.trigger_count,
        last_triggered_at: r.last_triggered_at.map(|dt| {
            dt.timestamp_millis()
        }),
        created_at: r.created_at.timestamp_millis(),
    }).collect())
}

/// 啟用或停用一個技能規範。
#[tauri::command]
pub async fn toggle_agent_skill(
    state: State<'_, AppState>,
    skill_id: String,
    is_active: bool,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    state.db.query(
        "UPDATE agent_skills SET is_active = $active \
         WHERE vault_id = $vid AND skill_id = $sid"
    )
    .bind(("active", is_active))
    .bind(("vid", vault_id))
    .bind(("sid", skill_id))
    .await.map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// 刪除一個技能規範。
#[tauri::command]
pub async fn delete_agent_skill(
    state: State<'_, AppState>,
    skill_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    state.db.query(
        "DELETE FROM agent_skills WHERE vault_id = $vid AND skill_id = $sid"
    )
    .bind(("vid", vault_id))
    .bind(("sid", skill_id))
    .await.map_err(|e| AppError::Database(e.to_string()))?;
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
    let db = &state.db;

    // 載入知識項目
    #[derive(Deserialize)]
    struct KIRow { title: String, ai_summary: String, source_refs: Option<serde_json::Value> }
    let mut resp = db.query(
        "SELECT title, ai_summary, source_refs FROM knowledge_items \
         WHERE vault_id = $vid AND item_id = $iid LIMIT 1"
    )
    .bind(("vid", vault_id.clone())).bind(("iid", item_id.clone()))
    .await.map_err(|e| AppError::Database(e.to_string()))?;
    let row = resp.take::<Vec<KIRow>>(0)
        .map_err(|e| AppError::Database(format!("KIRow deserialize error: {e}")))?
        .into_iter().next()
        .ok_or_else(|| AppError::Import(format!("knowledge item not found: {}", item_id)))?;

    let source_refs: Vec<KnowledgeRef> = row.source_refs
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let refs_text = source_refs.iter()
        .map(|r| format!("- [{}]({})", r.title, r.path))
        .collect::<Vec<_>>().join("\n");

    let system_prompt = r#"你是知識管理 AI 助理，專門協助使用者將知識轉化為「可程式化的個人 AI 助理」能力。
根據提供的知識內容，回傳嚴格的 JSON 物件（不要有任何其他文字）：

{
  "note_cards": [
    {
      "title": "卡片標題",
      "template": "concept | procedure | reference",
      "content": "完整 markdown（含 frontmatter，格式參考如下）",
      "reason": "為什麼值得建立這張卡片"
    }
  ],
  "skill_cards": [
    {
      "title": "技能標題",
      "trigger": "當問題涉及 X、Y、Z 時（描述觸發此技能的情境）",
      "behavior": "具體的操作指令：先做A，再做B，最後C（agent 應遵循的行為規範）",
      "auto_tool_calls": ["search_vault"]
    }
  ]
}

規則：
- note_cards：2-3 張，template 限 concept/procedure/reference
- skill_cards：1-2 張
  - trigger 必須明確描述觸發情境，以「當…時」開頭
  - behavior 必須是可執行的操作指令，不能是模糊描述
  - auto_tool_calls 只能包含：search_vault、read_note、list_structure（或空陣列）
  - 若知識內容不適合產生 skill_cards，可回傳空陣列

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

- 原始知識標題"#;

    let user_content = format!(
        "## 標題\n{}\n\n## AI 整理摘要\n{}\n\n## 來源\n{}",
        row.title, row.ai_summary, refs_text
    );

    let base_url = crate::commands::ai::ensure_server_running(state.inner(), &app).await
        .map_err(|e| AppError::AI(e.to_string()))?;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_content},
        ],
        "stream": false,
        "temperature": 0.3,
        "max_tokens": 1500,
    });
    let response = client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(std::time::Duration::from_secs(90))
        .send().await.map_err(|e| AppError::AI(e.to_string()))?;

    let json: serde_json::Value = response.json().await
        .map_err(|e| AppError::AI(format!("回應解析失敗：{}", e)))?;
    let raw = json["choices"][0]["message"]["content"]
        .as_str().unwrap_or("").trim().to_string();

    // 找 JSON 物件邊界（LLM 可能夾雜說明文字）
    let preview: String = raw.chars().take(500).collect();
    eprintln!("[suggest_kb_cards] raw LLM response ({} chars): {}", raw.len(), preview);
    let obj_start = raw.find('{').unwrap_or(0);
    let obj_end = raw.rfind('}').map(|i| i + 1).unwrap_or(raw.len());
    let json_str = &raw[obj_start..obj_end];

    let suggestions: KBAndSkillSuggestions = serde_json::from_str(json_str)
        .unwrap_or_else(|e| {
            let js_preview: String = json_str.chars().take(300).collect();
            eprintln!("[suggest_kb_cards] JSON parse error: {e}\njson_str: {}", js_preview);
            KBAndSkillSuggestions { note_cards: vec![], skill_cards: vec![] }
        });

    // 持久化 note_cards 到 kb_suggestions（清除同 item 的舊建議）
    let _ = db.query(
        "DELETE FROM kb_suggestions WHERE vault_id = $vid AND page_id = $pid"
    ).bind(("vid", vault_id.clone())).bind(("pid", item_id.clone())).await;

    let now_ms = chrono::Local::now().timestamp_millis();
    for s in &suggestions.note_cards {
        let sid = uuid::Uuid::new_v4().to_string();
        let _ = db.query(
            "INSERT INTO kb_suggestions \
             (suggestion_id, vault_id, session_id, page_id, title, template, content, reason, created_at) \
             VALUES ($sid, $vid, $sess, $pid, $title, $tmpl, $content, $reason, $now)"
        )
        .bind(("sid", sid))
        .bind(("vid", vault_id.clone()))
        .bind(("sess", item_id.clone()))
        .bind(("pid", item_id.clone()))
        .bind(("title", s.title.clone()))
        .bind(("tmpl", s.template.clone()))
        .bind(("content", s.content.clone()))
        .bind(("reason", s.reason.clone()))
        .bind(("now", now_ms))
        .await;
    }

    // 持久化 skill_cards：清除舊建議再重新插入，並計算 trigger embedding
    let _ = db.query(
        "DELETE FROM agent_skills WHERE vault_id = $vid AND knowledge_item_id = $kid"
    ).bind(("vid", vault_id.clone())).bind(("kid", item_id.clone())).await;

    let emb_url: Option<String> = {
        let port = *state.embedding_actual_port.lock().await;
        port.map(|p| format!("http://127.0.0.1:{}", p))
    };

    let allowed = ["search_vault", "read_note", "list_structure"];
    let mut saved_skills: Vec<AgentSkillRecord> = Vec::new();

    for skill in &suggestions.skill_cards {
        let skill_id = uuid::Uuid::new_v4().to_string();
        let safe_tools: Vec<String> = skill.auto_tool_calls.iter()
            .filter(|t| allowed.contains(&t.as_str()))
            .cloned().collect();

        // 計算 trigger embedding（若 embedding server 就緒）
        let trigger_embedding: Option<Vec<f32>> = if let Some(ref url) = emb_url {
            let vec = crate::commands::ai::get_embedding(&client, url, &skill.trigger).await;
            if vec.is_empty() { None } else { Some(vec) }
        } else { None };

        let mode = if skill.injection_mode == "active" { "active" } else { "passive" };
        let scope = valid_scope(&skill.agent_scope).to_string();
        let insert_result = db.query(
            "INSERT INTO agent_skills \
             (skill_id, vault_id, knowledge_item_id, title, trigger, behavior, \
              auto_tool_calls, is_active, injection_mode, agent_scope, trigger_count, trigger_embedding, created_at) \
             VALUES ($sid, $vid, $kid, $title, $trigger, $behavior, \
                     $tools, false, $mode, $scope, 0, $emb, time::now())"
        )
        .bind(("sid", skill_id.clone()))
        .bind(("vid", vault_id.clone()))
        .bind(("kid", item_id.clone()))
        .bind(("title", skill.title.clone()))
        .bind(("trigger", skill.trigger.clone()))
        .bind(("behavior", skill.behavior.clone()))
        .bind(("tools", safe_tools.clone()))
        .bind(("mode", mode.to_string()))
        .bind(("scope", scope.clone()))
        .bind(("emb", trigger_embedding))
        .await;
        if let Err(e) = insert_result {
            eprintln!("[suggest_kb_cards] INSERT agent_skills FAILED: {e}");
        }

        saved_skills.push(AgentSkillRecord {
            skill_id,
            vault_id: vault_id.clone(),
            knowledge_item_id: item_id.clone(),
            title: skill.title.clone(),
            trigger: skill.trigger.clone(),
            behavior: skill.behavior.clone(),
            auto_tool_calls: safe_tools,
            is_active: false,
            injection_mode: skill.injection_mode.clone(),
            agent_scope: scope,
            trigger_count: 0,
            last_triggered_at: None,
            created_at: now_ms,
        });
    }

    // 發出 kb:suggestions_ready 事件（取代舊的 kb:suggestion_token / kb:suggestion_done）
    let _ = app.emit("kb:suggestions_ready", serde_json::json!({
        "item_id": &item_id,
        "note_cards": &suggestions.note_cards,
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
    let db = &state.db;

    // 解析訊息，只保留 user/assistant
    #[derive(Deserialize)]
    struct RawMsg { role: String, content: String }
    let raw_msgs: Vec<RawMsg> = serde_json::from_str(&messages_json)
        .map_err(|e| AppError::Import(format!("messages_json 解析失敗：{}", e)))?;
    let filtered: Vec<&RawMsg> = raw_msgs.iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .collect();
    if filtered.is_empty() {
        return Err(AppError::Import("對話內容為空，無法壓縮".into()));
    }

    // 組成對話文字供 LLM 閱讀
    let conv_text = filtered.iter().map(|m| {
        let role_label = if m.role == "user" { "使用者" } else { "助理" };
        format!("**{}**：{}", role_label, m.content)
    }).collect::<Vec<_>>().join("\n\n");

    let system_prompt = r#"你是個人知識萃取 AI，專門從對話中萃取「使用者可程式化 AI 助理」所需的行為規則。
分析這段對話，回傳嚴格 JSON（不含其他文字）：

{
  "title": "這段對話的簡短標題（10字以內）",
  "knowledge_summary": "敘述性摘要，描述討論了什麼、決定了什麼、使用者的背景脈絡（供未來語意搜尋）",
  "skill_candidates": [
    {
      "title": "技能標題",
      "trigger": "當使用者問到...時（具體描述觸發情境）",
      "behavior": "應先...，再...，最後...（可執行的操作指令）",
      "auto_tool_calls": []
    }
  ]
}

萃取優先順序：
1. 使用者明確表達的偏好（回答格式、深度、風格）
2. 已做出的決策（技術選型、方向）→ 轉成「不需重複評估 X，直接用 Y」的行為規則
3. 隱性工作習慣（從對話行為推斷）→ 轉成可執行規則
4. 高密度 Q&A（問題有意義 + 答案有知識價值）

skill_candidates：只萃取能直接改變未來 AI 行為的規則。若對話純屬閒聊或無可萃取規則，回傳空陣列。
auto_tool_calls 只能包含：search_vault、read_note、list_structure（或空陣列）。"#;

    let base_url = ensure_server_running(state.inner(), &app).await
        .map_err(|e| AppError::AI(e.to_string()))?;
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": conv_text},
        ],
        "stream": false,
        "temperature": 0.2,
        "max_tokens": 1200,
    });
    let response = client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(std::time::Duration::from_secs(90))
        .send().await.map_err(|e| AppError::AI(e.to_string()))?;

    let json: serde_json::Value = response.json().await
        .map_err(|e| AppError::AI(format!("回應解析失敗：{}", e)))?;
    let raw = json["choices"][0]["message"]["content"]
        .as_str().unwrap_or("").trim().to_string();

    let obj_start = raw.find('{').unwrap_or(0);
    let obj_end = raw.rfind('}').map(|i| i + 1).unwrap_or(raw.len());
    let json_str = &raw[obj_start..obj_end];

    let compression: ConvCompression = serde_json::from_str(json_str)
        .unwrap_or_else(|e| {
            eprintln!("[compress_conv] JSON parse error: {e}");
            ConvCompression {
                title: "對話壓縮".into(),
                knowledge_summary: conv_text.chars().take(500).collect(),
                skill_candidates: vec![],
            }
        });

    // ── 建立 knowledge item ────────────────────────────────────────────────────
    let item_id = Uuid::new_v4().to_string();
    let now_ms = chrono::Local::now().timestamp_millis();
    let source_refs: Vec<KnowledgeRef> = vec![];  // 對話來源，無外部 URL

    db.query(
        "INSERT INTO knowledge_items \
         (vault_id, item_id, session_id, title, source_refs, ai_summary, created_at) \
         VALUES ($vid, $iid, $sid, $title, $refs, $summary, time::now())"
    )
    .bind(("vid", vault_id.clone())).bind(("iid", item_id.clone()))
    .bind(("sid", "conversation".to_string()))
    .bind(("title", compression.title.clone()))
    .bind(("refs", source_refs))
    .bind(("summary", compression.knowledge_summary.clone()))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    // 背景：chunk + embed 知識摘要
    let emb_url: Option<String> = {
        let port = *state.embedding_actual_port.lock().await;
        port.map(|p| format!("http://127.0.0.1:{}", p))
    };
    {
        let db2 = db.clone();
        let vid2 = vault_id.clone();
        let iid2 = item_id.clone();
        let summary2 = compression.knowledge_summary.clone();
        let emb_url2 = emb_url.clone();
        tokio::spawn(async move {
            let file_path = format!("knowledge_items/{}.md", iid2);
            let chunks = crate::vault::chunker::chunk_note(&file_path, &summary2, now_ms);
            for chunk in &chunks {
                let _ = db2.query(
                    "INSERT INTO chunks \
                     (vault_id, chunk_id, file_path, section, content, links, chunk_type, word_count, updated_at, item_id) \
                     VALUES ($vid, $cid, $fp, $section, $content, $links, $chunk_type, $wc, time::now(), $iid) \
                     ON DUPLICATE KEY UPDATE content = $content, item_id = $iid, updated_at = time::now()"
                )
                .bind(("vid", vid2.clone())).bind(("cid", chunk.id.clone()))
                .bind(("fp", chunk.file_path.clone())).bind(("section", chunk.section.clone()))
                .bind(("content", chunk.content.clone())).bind(("links", chunk.links.clone()))
                .bind(("chunk_type", chunk.chunk_type.clone())).bind(("wc", chunk.word_count))
                .bind(("iid", iid2.clone()))
                .await.ok();
            }
            if let Some(ref url) = emb_url2 {
                let _ = crate::vault::chunker::upsert_chunks(&db2, &vid2, &chunks, Some(url)).await;
            }
        });
    }

    // ── 持久化 skill_candidates ────────────────────────────────────────────────
    let allowed = ["search_vault", "read_note", "list_structure"];
    let mut saved_skills: Vec<AgentSkillRecord> = Vec::new();

    for skill in &compression.skill_candidates {
        let skill_id = Uuid::new_v4().to_string();
        let safe_tools: Vec<String> = skill.auto_tool_calls.iter()
            .filter(|t| allowed.contains(&t.as_str()))
            .cloned().collect();

        let trigger_embedding: Option<Vec<f32>> = if let Some(ref url) = emb_url {
            let vec = get_embedding(&client, url, &skill.trigger).await;
            if vec.is_empty() { None } else { Some(vec) }
        } else { None };

        let mode = if skill.injection_mode == "active" { "active" } else { "passive" };
        let scope = valid_scope(&skill.agent_scope).to_string();
        let _ = db.query(
            "INSERT INTO agent_skills \
             (skill_id, vault_id, knowledge_item_id, title, trigger, behavior, \
              auto_tool_calls, is_active, injection_mode, agent_scope, trigger_count, trigger_embedding, created_at) \
             VALUES ($sid, $vid, $kid, $title, $trigger, $behavior, \
                     $tools, false, $mode, $scope, 0, $emb, time::now())"
        )
        .bind(("sid", skill_id.clone())).bind(("vid", vault_id.clone()))
        .bind(("kid", item_id.clone())).bind(("title", skill.title.clone()))
        .bind(("trigger", skill.trigger.clone())).bind(("behavior", skill.behavior.clone()))
        .bind(("tools", safe_tools.clone())).bind(("mode", mode.to_string()))
        .bind(("scope", scope.clone())).bind(("emb", trigger_embedding))
        .await;

        saved_skills.push(AgentSkillRecord {
            skill_id,
            vault_id: vault_id.clone(),
            knowledge_item_id: item_id.clone(),
            title: skill.title.clone(),
            trigger: skill.trigger.clone(),
            behavior: skill.behavior.clone(),
            auto_tool_calls: safe_tools,
            is_active: false,
            injection_mode: skill.injection_mode.clone(),
            agent_scope: scope,
            trigger_count: 0,
            last_triggered_at: None,
            created_at: now_ms,
        });
    }

    // emit kb:suggestions_ready（note_cards 為空，因為壓縮不產生筆記卡片）
    let _ = app.emit("kb:suggestions_ready", serde_json::json!({
        "item_id": &item_id,
        "note_cards": serde_json::json!([]),
        "skill_cards": &saved_skills,
    }));

    Ok(CompressedConvResult {
        item_id,
        title: compression.title,
        skill_count: saved_skills.len(),
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
    let base_url = ensure_server_running(state.inner(), &app).await
        .map_err(|e| AppError::AI(e.to_string()))?;
    let client = reqwest::Client::new();

    let system_prompt = r#"你是個人 AI 行為規則萃取器。根據以下對話交換，萃取一條可重用的技能規範。
回傳嚴格 JSON（不含其他文字、不含 markdown code fence）：

{
  "title": "技能標題（10字以內）",
  "trigger": "當使用者...時（具體觸發條件，15-30字）",
  "behavior": "應先...，再...（具體、可執行的操作指令）",
  "auto_tool_calls": []
}

auto_tool_calls 只能包含：search_vault、read_note、list_structure（或空陣列）。
若對話內容無可萃取的行為規則，trigger 欄填入「無法萃取」。"#;

    let conv = format!("使用者：{}\n\n助理：{}", user_msg, assistant_msg);
    let body = serde_json::json!({
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": conv},
        ],
        "stream": false,
        "temperature": 0.2,
        "max_tokens": 400,
    });

    let response = client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(std::time::Duration::from_secs(60))
        .send().await.map_err(|e| AppError::AI(e.to_string()))?;

    let json: serde_json::Value = response.json().await
        .map_err(|e| AppError::AI(format!("回應解析失敗：{}", e)))?;
    let raw = json["choices"][0]["message"]["content"]
        .as_str().unwrap_or("").trim().to_string();

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
    let db = &state.db;
    let mut out = format!("=== debug_kb_chunks ===\nvault_id: {}\n\n", vault_id);

    // ── Import sessions ──────────────────────────────────────────────────────
    #[derive(serde::Deserialize)]
    struct SessRow { session_id: String, seed_url: String, site_name: String, status: String }
    let mut rs = db.query(
        "SELECT session_id, seed_url, site_name, status FROM import_sessions WHERE vault_id = $vid LIMIT 10"
    ).bind(("vid", vault_id.clone())).await.map_err(|e| AppError::Database(e.to_string()))?;
    let sessions: Vec<SessRow> = rs.take(0).unwrap_or_default();
    out += &format!("--- import_sessions ({} 筆) ---\n", sessions.len());
    for s in &sessions {
        out += &format!("  [{}] {} | {} | status={}\n", s.session_id, s.site_name, s.seed_url, s.status);
    }
    out += "\n";

    // ── Import pages per session ──────────────────────────────────────────────
    #[derive(serde::Deserialize)]
    struct PageDebugRow {
        page_id: String,
        session_id: String,
        url: String,
        title: String,
        status: String,
        content_md: Option<String>,
        #[allow(dead_code)]
        last_crawled: Option<serde_json::Value>,
    }
    let mut rp = db.query(
        "SELECT page_id, session_id, url, title, status, content_md, last_crawled \
         FROM import_pages WHERE vault_id = $vid ORDER BY last_crawled DESC LIMIT 30"
    ).bind(("vid", vault_id.clone())).await.map_err(|e| AppError::Database(e.to_string()))?;
    let pages: Vec<PageDebugRow> = rp.take(0).unwrap_or_default();
    out += &format!("--- import_pages ({} 筆, newest first) ---\n", pages.len());
    for p in &pages {
        let md_info = match &p.content_md {
            None => "content_md=None".to_string(),
            Some(s) if s.is_empty() => "content_md=Some(\"\")".to_string(),
            Some(s) => format!("content_md_len={}", s.len()),
        };
        out += &format!(
            "  [{}] sess={} | status={} | {} | {}\n  title: {}\n",
            p.page_id, p.session_id, p.status, md_info, p.url, p.title
        );
    }
    out += "\n";

    // ── Chunks ───────────────────────────────────────────────────────────────
    #[derive(serde::Deserialize)]
    struct StatusCount { status: String, count: i64 }
    let mut r1 = db.query(
        "SELECT status, count() AS count FROM chunks WHERE vault_id = $vid GROUP BY status"
    ).bind(("vid", vault_id.clone())).await.map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<StatusCount> = r1.take(0).unwrap_or_default();
    if rows.is_empty() {
        out += "chunks: 無任何資料（vault 尚未建立 chunks 索引）\n";
    } else {
        out += "--- chunks 狀態分佈 ---\n";
        for r in &rows { out += &format!("  status={:?}  count={}\n", r.status, r.count); }
    }
    out += "\n";

    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct SampleChunk { file_path: String, section: String, status: String, updated_at: Option<serde_json::Value> }
    let mut r2 = db.query(
        "SELECT file_path, section, status, updated_at FROM chunks WHERE vault_id = $vid ORDER BY updated_at DESC LIMIT 5"
    ).bind(("vid", vault_id.clone())).await.map_err(|e| AppError::Database(e.to_string()))?;
    let samples: Vec<SampleChunk> = r2.take(0).unwrap_or_default();
    out += &format!("--- 最近 {} 筆 chunks ---\n", samples.len());
    for s in &samples {
        out += &format!("  {} [{}] status={:?}\n", s.file_path, s.section, s.status);
    }
    Ok(out)
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
                "SELECT file_path, section, content,
                        vector::similarity::cosine(embedding, $qvec) AS score
                 FROM chunks
                 WHERE vault_id = $vid AND embedding != NONE AND status = 'verified'
                 ORDER BY score DESC LIMIT 8"
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

    // 3. STRICT system prompt（依題型選擇一般問答或跨筆記推理）
    let is_cross_note = {
        let q = question.to_lowercase();
        q.contains("比較") || q.contains("對比") || q.contains("異同") || q.contains("差異")
        || q.contains("總結") || q.contains("綜合") || q.contains("差別") || q.contains("共同")
        || q.contains("相同") || q.contains("不同") || q.contains("compare") || q.contains("synthesize")
    };

    // Emit a hint so UI can show cross-note mode indicator
    if is_cross_note {
        let _ = app.emit("knowledge:cross_note", serde_json::json!({ "query_id": query_id }));
    }

    let context = chunks.iter().enumerate().map(|(i, c)| {
        let loc = if c.section.is_empty() {
            c.file_path.clone()
        } else {
            format!("{} § {}", c.file_path, c.section)
        };
        let excerpt: String = c.content.chars().take(1500).collect();
        format!("[{}] 來源：{}\n{}", i + 1, loc, excerpt)
    }).collect::<Vec<_>>().join("\n\n---\n\n");

    let system = if is_cross_note {
        format!(
            "你是知識庫跨筆記推理助手。\
            規則：\
            1. 根據以下多個「知識庫片段」進行比較、對比或綜合分析。\
            2. 每個陳述必須以 [1]、[2] 等格式標示來源編號。\
            3. 若有多個來源可以比較，請用結構化方式（如表格或對比清單）呈現。\
            4. 若知識庫片段中找不到足夠資訊，必須明確說明。\
            5. 用繁體中文回答。\n\n知識庫片段：\n\n{}",
            context
        )
    } else {
        format!(
            "你是嚴格的知識庫問答助手。\
            規則：\
            1. 只能根據以下「知識庫片段」回答，禁止使用訓練資料中的知識。\
            2. 每個陳述必須以 [1]、[2] 等格式標示來源編號。\
            3. 若知識庫片段中找不到答案，必須明確說「知識庫中沒有此資訊」，不得猜測或補充。\
            4. 用繁體中文回答。\n\n知識庫片段：\n\n{}",
            context
        )
    };

    // 4. AI streaming
    let provider = queries::get_setting(db, "ai_provider").await.unwrap_or_default().unwrap_or_default();
    let model = queries::get_setting(db, "ai_model").await.unwrap_or_default().unwrap_or_default();
    let api_key = read_api_key(&app_state.api_key_cache, &provider).await;

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
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;

    #[derive(serde::Deserialize)]
    struct NoteRow { file_path: String, status: String, updated_at: i64 }

    let mut r = db.query(
        "SELECT file_path, status, updated_at FROM chunks \
         WHERE vault_id = $vid \
         GROUP BY file_path, status, updated_at"
    ).bind(("vid", vault_id.clone()))
    .await.map_err(|e| AppError::Database(e.to_string()))?;
    let all_chunks: Vec<NoteRow> = r.take(0).unwrap_or_default();

    use std::collections::HashMap;
    let mut note_map: HashMap<String, NoteRow> = HashMap::new();
    for row in all_chunks {
        note_map.entry(row.file_path.clone()).or_insert(row);
    }
    let notes: Vec<NoteRow> = note_map.into_values().collect();

    let total_notes = notes.len() as i64;
    let mut verified = 0i64;
    let mut draft = 0i64;
    let mut deprecated = 0i64;
    let mut no_status = 0i64;

    let mut topic_map: HashMap<String, i64> = HashMap::new();

    use chrono::{Local, Duration, NaiveDate};
    let today = Local::now().date_naive();
    let cutoff_ms = (Local::now() - Duration::days(30)).timestamp_millis();
    let mut day_total: HashMap<NaiveDate, i64> = HashMap::new();
    let mut day_verified: HashMap<NaiveDate, i64> = HashMap::new();

    for note in &notes {
        match note.status.as_str() {
            "verified" => verified += 1,
            "draft" => draft += 1,
            "deprecated" => deprecated += 1,
            _ => no_status += 1,
        }

        let topic = note.file_path
            .trim_start_matches('/')
            .split('/')
            .next()
            .unwrap_or("其他")
            .to_string();
        *topic_map.entry(topic).or_insert(0) += 1;

        if note.updated_at >= cutoff_ms {
            let dt = chrono::DateTime::from_timestamp_millis(note.updated_at)
                .map(|d| d.with_timezone(&Local).date_naive())
                .unwrap_or(today);
            *day_total.entry(dt).or_insert(0) += 1;
            if note.status == "verified" {
                *day_verified.entry(dt).or_insert(0) += 1;
            }
        }
    }

    let mut topics: Vec<KBTopic> = topic_map.into_iter()
        .map(|(name, count)| KBTopic { name, count })
        .collect();
    topics.sort_by(|a, b| b.count.cmp(&a.count));
    topics.truncate(10);

    let daily_trend: Vec<KBDayEntry> = (0..30i64).map(|i| {
        let d = today - Duration::days(29 - i);
        let total = *day_total.get(&d).unwrap_or(&0);
        let v = *day_verified.get(&d).unwrap_or(&0);
        KBDayEntry { date: d.format("%Y-%m-%d").to_string(), total, verified: v }
    }).collect();

    Ok(KBStats {
        total_notes,
        verified,
        draft,
        deprecated,
        no_status,
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
#[tauri::command]
pub async fn get_aging_notes(
    state: State<'_, AppState>,
    threshold_days: Option<i64>,
) -> Result<Vec<AgingNote>, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let days = threshold_days.unwrap_or(30);

    use chrono::Local;
    let now_ms = Local::now().timestamp_millis();
    let cutoff_ms = now_ms - days * 24 * 60 * 60 * 1000;

    #[derive(serde::Deserialize)]
    struct ChunkAging {
        file_path: String,
        updated_at: i64,
        reviewed_at: Option<i64>,
    }

    let mut r = db.query(
        "SELECT file_path, updated_at, reviewed_at FROM chunks \
         WHERE vault_id = $vid AND status = 'verified' \
         GROUP BY file_path, updated_at, reviewed_at"
    ).bind(("vid", vault_id.clone()))
    .await.map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<ChunkAging> = r.take(0).unwrap_or_default();

    // Deduplicate per file_path; use reviewed_at if set, else updated_at
    use std::collections::HashMap;
    let mut note_map: HashMap<String, ChunkAging> = HashMap::new();
    for row in rows {
        note_map.entry(row.file_path.clone()).or_insert(row);
    }

    let mut aging: Vec<AgingNote> = note_map.into_values()
        .filter_map(|row| {
            let effective_ts = row.reviewed_at.unwrap_or(row.updated_at);
            if effective_ts <= cutoff_ms {
                let days_since = (now_ms - effective_ts) / (24 * 60 * 60 * 1000);
                let title = row.file_path
                    .split('/').last().unwrap_or("")
                    .trim_end_matches(".md")
                    .to_string();
                Some(AgingNote {
                    file_path: row.file_path,
                    title,
                    days_since_review: days_since,
                    reviewed_at: row.reviewed_at,
                })
            } else {
                None
            }
        })
        .collect();

    aging.sort_by(|a, b| b.days_since_review.cmp(&a.days_since_review));
    Ok(aging)
}

/// 標記筆記為「已審查」（更新 reviewed_at）
#[tauri::command]
pub async fn mark_note_reviewed(
    state: State<'_, AppState>,
    path: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    use chrono::Local;
    let now_ms = Local::now().timestamp_millis();
    state.db.query(
        "UPDATE chunks SET reviewed_at = $ts WHERE vault_id = $vid AND file_path = $fp"
    )
    .bind(("ts", now_ms))
    .bind(("vid", vault_id))
    .bind(("fp", path))
    .await.map_err(|e| AppError::Database(e.to_string()))?;
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
pub async fn get_brave_used(db: &SurrealDb, key_id: &str) -> u32 {
    let month_key = format!("brave_search_month_{}", key_id);
    let used_key  = format!("brave_search_used_{}", key_id);
    let stored_month = queries::get_setting(db, &month_key)
        .await.ok().flatten().unwrap_or_default();
    if stored_month != current_month_str() {
        return 0;
    }
    queries::get_setting(db, &used_key)
        .await.ok().flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Increment the monthly Brave search counter for the given API key,
/// resetting automatically when the month changes.
async fn increment_brave_used(db: &SurrealDb, key_id: &str) {
    let month_key = format!("brave_search_month_{}", key_id);
    let used_key  = format!("brave_search_used_{}", key_id);
    let month = current_month_str();
    let stored_month = queries::get_setting(db, &month_key)
        .await.ok().flatten().unwrap_or_default();
    let new_used = if stored_month != month {
        let _ = queries::set_setting(db, &month_key, &month).await;
        1u32
    } else {
        get_brave_used(db, key_id).await + 1
    };
    let _ = queries::set_setting(db, &used_key, &new_used.to_string()).await;
}

/// Read Brave Search API key from OS keyring.
fn read_brave_api_key() -> Option<String> {
    keyring::Entry::new("com.notetreelm.app", "brave_search")
        .ok()
        .and_then(|e| e.get_password().ok())
        .filter(|k| !k.is_empty())
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
    db: SurrealDb,
    vault_id: String,
    query: String,
    urls: Vec<(String, String)>, // (url, title)
    app: AppHandle,
    emb_url: Option<String>,
) {
    if urls.is_empty() {
        return;
    }

    let session_id = Uuid::new_v4().to_string();
    let q_short: String = query.chars().take(30).collect();
    let site_name = format!("搜尋：{}", q_short);
    let root_folder = format!("imports/search-{}", &session_id[..8]);

    // Create import session
    if db
        .query(
            "INSERT INTO import_sessions \
             (vault_id, session_id, conversation_id, seed_url, site_name, root_folder, status, created_at, updated_at) \
             VALUES ($vid, $sid, '', $seed, $sname, $rfolder, 'active', time::now(), time::now())",
        )
        .bind(("vid", vault_id.clone()))
        .bind(("sid", session_id.clone()))
        .bind(("seed", format!("search:{}", query)))
        .bind(("sname", site_name.clone()))
        .bind(("rfolder", root_folder.clone()))
        .await
        .is_err()
    {
        return;
    }

    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; noteTreeLM/0.1; knowledge-import)")
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };

    let now_date = chrono::Utc::now().format("%Y-%m-%d").to_string();

    for (url, title) in &urls {
        // Security check: block internal addresses
        if validate_url(url).is_err() {
            continue;
        }

        let page_id = Uuid::new_v4().to_string();

        let _ = db
            .query(
                "INSERT INTO import_pages \
                 (vault_id, page_id, session_id, url, title, depth, status, created_at) \
                 VALUES ($vid, $pid, $sid, $url, $title, 0, 'pending', time::now())",
            )
            .bind(("vid", vault_id.clone()))
            .bind(("pid", page_id.clone()))
            .bind(("sid", session_id.clone()))
            .bind(("url", url.clone()))
            .bind(("title", title.clone()))
            .await;

        // Fetch and convert page
        let html = match client.get(url).send().await {
            Ok(resp) => match resp.text().await {
                Ok(t) => t,
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        let actual_title = {
            let t = extract_title(&html);
            if t.is_empty() { title.clone() } else { t }
        };

        let body_md = html_to_markdown_rich(&html);
        let note_content = format!(
            "---\ntitle: {}\nsource: {}\nimported: {}\nstatus: verified\n---\n\n{}\n",
            actual_title, url, now_date, body_md
        );

        let slug = slugify(&actual_title);
        let note_path = format!("{}/{}.md", root_folder, slug);
        let new_hash = sha256_hex(&note_content);

        // Store chunks
        let now_ms = chrono::Utc::now().timestamp_millis();
        let chunks = crate::vault::chunker::chunk_note(&note_path, &note_content, now_ms);
        let _ = crate::vault::chunker::upsert_chunks(&db, &vault_id, &chunks, emb_url.as_deref()).await;

        // Update import_pages record
        let now_dt = surrealdb::sql::Datetime::from(chrono::Utc::now());
        let _ = db
            .query(
                "UPDATE import_pages SET status = 'imported', note_path = $path, content_md = $content, \
                 content_hash = $hash, last_crawled = $now \
                 WHERE vault_id = $vid AND page_id = $pid",
            )
            .bind(("path", note_path))
            .bind(("content", note_content))
            .bind(("hash", new_hash))
            .bind(("now", now_dt))
            .bind(("vid", vault_id.clone()))
            .bind(("pid", page_id))
            .await;
    }

    // Notify frontend so Import Center can refresh
    let _ = app.emit(
        "import:session_created",
        serde_json::json!({
            "session_id": session_id,
            "site_name": site_name,
        }),
    );
}

/// Web search tool called by the Agent.
/// Searches via Brave Search API and spawns a background import of the top results.
pub async fn tool_web_search(
    db: &SurrealDb,
    vault_id: &str,
    query: &str,
    app: &AppHandle,
    emb_url: Option<&str>,
) -> String {
    let api_key = match read_brave_api_key() {
        Some(k) => k,
        None => return "請至設定頁面設定 Brave Search API Key".to_string(),
    };
    let key_id = brave_key_id(&api_key);

    // Check monthly quota before making the request
    let used = get_brave_used(db, &key_id).await;
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
        increment_brave_used(db, &key_id).await;
    }

    if results.is_empty() {
        return format!(
            "Brave Search 未回傳「{}」的搜尋結果（回應成功但結果為空）。",
            query
        );
    }

    // Spawn background import (non-blocking)
    {
        let top_urls: Vec<(String, String)> = results
            .iter()
            .take(3)
            .map(|(title, url, _)| (url.clone(), title.clone()))
            .collect();
        let db = db.clone();
        let vid = vault_id.to_string();
        let q = query.to_string();
        let app = app.clone();
        let emb = emb_url.map(str::to_string);
        tokio::spawn(async move {
            background_import_search_results(db, vid, q, top_urls, app, emb).await;
        });
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
        "搜尋「{}」的結果：\n\n{}\n\n（已在背景將搜尋結果加入「匯入知識」，稍後可在匯入中心查看。）",
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
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;

    #[derive(serde::Deserialize)]
    struct Row { title: String, url: String, content_md: Option<String> }

    let mut resp = db.query(
        "SELECT title, url, content_md FROM import_pages \
         WHERE vault_id = $vid AND url = $url LIMIT 1"
    )
    .bind(("vid", vault_id))
    .bind(("url", source_url))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    let row = resp.take::<Vec<Row>>(0).unwrap_or_default().into_iter().next();
    Ok(row.map(|r| CachedPage {
        title: r.title,
        url: r.url,
        content_md: r.content_md.unwrap_or_default(),
    }))
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
    queries::set_setting(&state.db, "brave_current_key_id", &kid).await?;
    Ok(())
}

#[tauri::command]
pub async fn get_brave_search_usage(state: State<'_, AppState>) -> Result<BraveUsageInfo, AppError> {
    // Read key_id from DB — no keychain access needed here
    let key_id = queries::get_setting(&state.db, "brave_current_key_id")
        .await.ok().flatten().unwrap_or_default();
    let used = if key_id.is_empty() { 0 } else { get_brave_used(&state.db, &key_id).await };
    Ok(BraveUsageInfo {
        used,
        limit: BRAVE_SEARCH_MONTHLY_LIMIT,
        reset_label: next_month_reset_label(),
    })
}
