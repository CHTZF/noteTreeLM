use crate::{error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use tauri::State;
use url::Url;

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportResult {
    pub note_path: String,
    pub title: String,
    pub was_duplicate: bool,
}

/// 驗證 URL 安全性（防止 SSRF）
fn validate_url(raw_url: &str) -> Result<Url, AppError> {
    let parsed = Url::parse(raw_url)?;

    // 只允許 http/https
    if !["http", "https"].contains(&parsed.scheme()) {
        return Err(AppError::Security(format!(
            "不支援的協定：{}，只允許 http/https",
            parsed.scheme()
        )));
    }

    // 阻擋內網地址
    if let Some(host) = parsed.host_str() {
        let blocked_patterns = [
            "localhost", "127.", "0.0.0.0", "::1",
            "192.168.", "10.", "172.16.", "172.17.", "172.18.",
            "172.19.", "172.20.", "172.21.", "172.22.", "172.23.",
            "172.24.", "172.25.", "172.26.", "172.27.", "172.28.",
            "172.29.", "172.30.", "172.31.", "169.254.",
        ];
        for pattern in &blocked_patterns {
            if host.starts_with(pattern) || host == *pattern {
                return Err(AppError::Security(format!(
                    "不允許存取內網地址：{}",
                    host
                )));
            }
        }
    }

    Ok(parsed)
}

#[tauri::command]
pub async fn import_url(
    state: State<'_, AppState>,
    url: String,
) -> Result<ImportResult, AppError> {
    let parsed_url = validate_url(&url)?;
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }

    // 使用 reqwest 擷取頁面（非 SPA 靜態頁面）
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; noteTreeLM/0.1)")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;

    let response = client.get(parsed_url.as_str()).send().await?;
    let html = response.text().await?;

    // 簡單的標題提取（從 <title> 和 <h1>）
    let title = extract_html_title(&html)
        .unwrap_or_else(|| parsed_url.host_str().unwrap_or("Imported Page").to_string());

    // HTML → 純文字（簡化版）
    let content = html_to_markdown(&html, &url, &title);

    // 建立筆記
    let note = super::vault::create_note(
        state.clone(),
        title.clone(),
        Some("imports".to_string()),
        Some(content),
    ).await?;

    Ok(ImportResult {
        note_path: note.path,
        title,
        was_duplicate: false,
    })
}

fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    if let Some(start) = lower.find("<title>") {
        if let Some(end) = lower[start..].find("</title>") {
            let title = &html[start + 7..start + end];
            let cleaned = title.trim().to_string();
            if !cleaned.is_empty() {
                return Some(cleaned);
            }
        }
    }
    None
}

fn html_to_markdown(html: &str, source_url: &str, title: &str) -> String {
    // 去除 script 和 style 標籤內容
    let without_scripts = remove_tags(html, "script");
    let without_style = remove_tags(&without_scripts, "style");

    // 去除所有 HTML 標籤
    let text = strip_html_tags(&without_style);

    // 清理多餘空白行
    let clean: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    let now = chrono::Utc::now().format("%Y-%m-%d").to_string();

    format!(
        "---\ntitle: {}\nsource: {}\nimported: {}\ntype: url-import\n---\n\n# {}\n\n{}\n",
        title,
        source_url,
        now,
        title,
        clean.join("\n\n")
    )
}

fn remove_tags(html: &str, tag: &str) -> String {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut result = String::new();
    let mut remaining = html;
    while let Some(start) = remaining.to_lowercase().find(&open) {
        result.push_str(&remaining[..start]);
        if let Some(end) = remaining[start..].to_lowercase().find(&close) {
            remaining = &remaining[start + end + close.len()..];
        } else {
            break;
        }
    }
    result.push_str(remaining);
    result
}

fn strip_html_tags(html: &str) -> String {
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
    // 解碼常見 HTML entities
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}
