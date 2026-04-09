//! User image (profile background) — a single free-text description per account
//! that accumulates long-lived personal context: location, language preference,
//! occupation, habits, etc.
//!
//! # Update flow
//! Called in the background after a conversation is saved (same trigger as
//! `maybe_trigger_memory_agent`). Uses a short-timeout LLM call to:
//!   1. Extract any new personal facts from the latest messages.
//!   2. Merge them with the existing user_image text.
//!   3. Upsert the result into `user_settings` under key `"user_image"`.
//!
//! # Injection flow
//! `load_user_image` is called in `build_agent_context` and the returned text
//! is prepended to the system prompt, bypassing semantic search entirely.

use serde_json::{json, Value};
use crate::db::SurrealDb;

const USER_IMAGE_KEY: &str = "user_image";
/// Max characters kept in user_image to avoid bloating the system prompt.
const MAX_USER_IMAGE_CHARS: usize = 800;

// ── Read ──────────────────────────────────────────────────────────────────────

/// Load the current user_image text for `account_id`.
/// Returns empty string if none exists.
pub(crate) async fn load_user_image(db: &SurrealDb, account_id: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Row { value: String }
    db.query("SELECT `value` FROM user_settings WHERE username = $u AND `key` = $k LIMIT 1")
        .bind(("u", account_id.to_string()))
        .bind(("k", USER_IMAGE_KEY.to_string()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .map(|r| r.value)
        .unwrap_or_default()
}

// ── Update ────────────────────────────────────────────────────────────────────

/// Background task: analyse `msgs` for personal facts and merge into user_image.
/// Uses a 6-second timeout — if LLM is busy serving chat, gives up silently.
/// Should be called via `tokio::spawn`.
pub(crate) async fn maybe_update_user_image(
    client:    reqwest::Client,
    llm_url:   String,
    db:        SurrealDb,
    account_id: String,
    msgs:      Vec<Value>,
) {
    if msgs.is_empty() || llm_url.is_empty() { return; }

    // Build conversation excerpt (last 10 messages, 150 chars each)
    let excerpt = msgs.iter().rev().take(10).collect::<Vec<_>>().into_iter().rev()
        .filter_map(|m| {
            let role = m["role"].as_str()?;
            let content: String = m["content"].as_str().unwrap_or("").chars().take(150).collect();
            if content.is_empty() { return None; }
            Some(format!("[{}]: {}", role, content))
        })
        .collect::<Vec<_>>()
        .join("\n");

    let existing = load_user_image(&db, &account_id).await;

    // Stage 1: fast keyword check — does conversation contain personal-info signals?
    let personal_keywords = ["我住", "我在", "我是", "我的", "我喜歡", "我不喜歡",
        "我工作", "我用", "I live", "I am", "I work", "I prefer", "my name",
        "我叫", "本人", "我們", "我家", "我平時"];
    let excerpt_lower = excerpt.to_lowercase();
    let has_personal_signal = personal_keywords.iter().any(|kw| excerpt_lower.contains(kw));
    if !has_personal_signal && !existing.is_empty() {
        // No personal signals and image already exists — skip to avoid unnecessary LLM calls
        return;
    }

    // Stage 2: LLM merge call
    let existing_section = if existing.is_empty() {
        "（目前尚無使用者背景資料）".to_string()
    } else {
        existing.clone()
    };

    let prompt = format!(
        "你是使用者形象提取器。根據以下對話，提取或更新使用者的個人背景資訊（居住地、語言、職業、喜好、習慣等）。\n\
        若對話無新資訊，直接回傳現有內容不做改動。\n\
        輸出格式：純文字，不超過 {} 字，不要解釋、不要標題、不要 markdown。\n\n\
        ### 現有使用者形象\n{}\n\n\
        ### 最新對話片段\n{}",
        MAX_USER_IMAGE_CHARS, existing_section, excerpt
    );

    let body = json!({
        "messages": [
            { "role": "user", "content": prompt }
        ],
        "stream": false,
        "temperature": 0.1,
        "max_tokens": 300,
    });

    let short_client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build()
    {
        Ok(c) => c,
        Err(_) => client,
    };

    let url = format!("{}/v1/chat/completions", llm_url.trim_end_matches('/'));
    let resp = match short_client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("[user_image] LLM call failed (likely busy): {}", e);
            return;
        }
    };

    let result: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return,
    };

    let new_image: String = result["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .chars()
        .take(MAX_USER_IMAGE_CHARS)
        .collect();

    if new_image.is_empty() || new_image == existing { return; }

    let now = chrono::Utc::now().timestamp();
    let _ = db
        .query("UPSERT user_settings SET username = $u, `key` = $k, `value` = $v, updated_at = $now WHERE username = $u AND `key` = $k")
        .bind(("u", account_id.clone()))
        .bind(("k", USER_IMAGE_KEY.to_string()))
        .bind(("v", new_image.clone()))
        .bind(("now", now))
        .await;

    tracing::info!("[user_image] updated for account={} ({} chars)", account_id, new_image.len());
}
