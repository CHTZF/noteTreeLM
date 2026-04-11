//! User image (profile background) — per-account, field-level persistent context.
//!
//! # Design
//! Two-key storage in `user_settings`:
//!   - `user_image`     : structured `欄位：值\n` text (consolidated, always injected)
//!   - `user_image_raw` : raw fact sentences appended from memory_agent routing
//!
//! When `user_image_raw` exceeds RAW_CONSOLIDATE_CHARS, a lightweight LLM call
//! merges raw + structured into a new structured `user_image` and clears raw.
//!
//! # Routing (called from save_memory_facts)
//! Each fact is embedded and compared to two anchor vectors:
//!   - PROFILE_ANCHOR  → closer means it's a personal attribute → route here
//!   - EPISODIC_ANCHOR → closer means it's an event/decision → route to memory_facts
//!
//! # Injection
//! `load_user_image` is called in `build_agent_context` and prepended to system
//! prompt so the model can complete queries that omit the subject
//! (e.g. "今天天氣如何" → model knows user is in 台北市).

use serde_json::{json, Value};
use crate::db::SurrealDb;

const USER_IMAGE_KEY: &str = "user_image";
const USER_IMAGE_RAW_KEY: &str = "user_image_raw";

/// Trigger consolidation when raw section exceeds this many chars.
const RAW_CONSOLIDATE_CHARS: usize = 400;

/// Semantic anchor for personal-attribute facts.
/// Covers all PROFILE_FIELDS dimensions for discriminative embedding comparison.
pub(crate) const PROFILE_ANCHOR: &str =
    "居住地點 所在城市 慣用語言 母語 回答風格偏好 溝通方式 說話習慣 \
     專業背景 技術能力 工作領域 時區 交通習慣 通勤方式 個人身份屬性";

/// Semantic anchor for episodic / event facts (decisions, tasks, technical choices…).
pub(crate) const EPISODIC_ANCHOR: &str =
    "決策事件 技術選型 任務進度 具體約定 過去發生的事 選擇結果 完成項目 歷史記錄";

/// Priority-ordered canonical field names rendered first in the structured profile.
pub(crate) const PROFILE_FIELDS: &[&str] = &[
    "所在地",
    "語言",
    "回答風格",   // 簡潔條列 / 詳細說明 / 口語 / 正式
    "專業背景",   // 比「職業」更細：含技術深度，e.g. "Rust/React 熟練，ML 初學者"
    "交通習慣",
    "時區",
];

// ── Field helpers ─────────────────────────────────────────────────────────────

type Fields = Vec<(String, String)>;

fn parse_fields(text: &str) -> Fields {
    text.lines()
        .filter_map(|line| {
            let pos = line.find('：')?;
            let key = line[..pos].trim().to_string();
            let val = line[pos + '：'.len_utf8()..].trim().to_string();
            if key.is_empty() || val.is_empty() { return None; }
            Some((key, val))
        })
        .collect()
}

fn render_fields(fields: &Fields) -> String {
    let mut lines: Vec<String> = Vec::new();
    for &pf in PROFILE_FIELDS {
        if let Some((_, val)) = fields.iter().find(|(k, _)| k == pf) {
            lines.push(format!("{}：{}", pf, val));
        }
    }
    for (key, val) in fields {
        if !PROFILE_FIELDS.contains(&key.as_str()) {
            lines.push(format!("{}：{}", key, val));
        }
    }
    lines.join("\n")
}

fn merge_fields(fields: &mut Fields, diff: &Value) {
    let Some(obj) = diff.as_object() else { return };
    for (k, v) in obj {
        let val = v.as_str().unwrap_or("").trim().to_string();
        if val.is_empty() {
            fields.retain(|(key, _)| key != k);
        } else if let Some(entry) = fields.iter_mut().find(|(key, _)| key == k) {
            entry.1 = val;
        } else {
            fields.push((k.clone(), val));
        }
    }
}

// ── DB helpers ────────────────────────────────────────────────────────────────

async fn load_setting(db: &SurrealDb, account_id: &str, key: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Row { value: String }
    db.query("SELECT `value` FROM user_settings WHERE username = $u AND `key` = $k LIMIT 1")
        .bind(("u", account_id.to_string()))
        .bind(("k", key.to_string()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .map(|r| r.value)
        .unwrap_or_default()
}

async fn upsert_setting(db: &SurrealDb, account_id: &str, key: &str, value: &str) {
    let now = chrono::Utc::now().timestamp();
    let _ = db
        .query("UPSERT user_settings SET username = $u, `key` = $k, `value` = $v, updated_at = $now WHERE username = $u AND `key` = $k")
        .bind(("u", account_id.to_string()))
        .bind(("k", key.to_string()))
        .bind(("v", value.to_string()))
        .bind(("now", now))
        .await;
}

// ── Read ──────────────────────────────────────────────────────────────────────

/// Load the current structured user_image text for `account_id`.
/// Returns empty string if none exists.
pub(crate) async fn load_user_image(db: &SurrealDb, account_id: &str) -> String {
    load_setting(db, account_id, USER_IMAGE_KEY).await
}

// ── Routing ───────────────────────────────────────────────────────────────────

/// Route a fact to profile (user_image) or episodic (memory_facts).
/// Returns `true` if fact should go to user_image.
/// Compares cosine similarity to both anchors; routes to the closer one.
/// Falls back to episodic if embeddings unavailable.
pub(crate) async fn is_profile_fact(
    client: &reqwest::Client,
    embedding_url: &Option<String>,
    fact: &str,
) -> bool {
    use crate::embedding::embedder::{embed_text, cosine_sim};

    let fact_emb = match embed_text(client, embedding_url, fact).await {
        Some(e) => e,
        None => return false, // no embedding → episodic fallback
    };
    let profile_emb = match embed_text(client, embedding_url, PROFILE_ANCHOR).await {
        Some(e) => e,
        None => return false,
    };
    let episodic_emb = match embed_text(client, embedding_url, EPISODIC_ANCHOR).await {
        Some(e) => e,
        None => return false,
    };

    let sim_profile  = cosine_sim(&fact_emb, &profile_emb);
    let sim_episodic = cosine_sim(&fact_emb, &episodic_emb);

    tracing::debug!(
        "[user_image] routing fact sim_profile={:.3} sim_episodic={:.3}: {:?}",
        sim_profile, sim_episodic,
        fact.chars().take(60).collect::<String>()
    );

    sim_profile > sim_episodic
}

// ── Append + Consolidate ──────────────────────────────────────────────────────

/// Append a raw fact sentence to `user_image_raw`.
/// If total raw chars exceed RAW_CONSOLIDATE_CHARS, triggers consolidation.
pub(crate) async fn append_raw_fact(
    client: reqwest::Client,
    llm_url: String,
    db: SurrealDb,
    account_id: String,
    fact: String,
) {
    let existing_raw = load_setting(&db, &account_id, USER_IMAGE_RAW_KEY).await;
    let new_raw = if existing_raw.is_empty() {
        fact.clone()
    } else {
        format!("{}\n{}", existing_raw, fact)
    };

    upsert_setting(&db, &account_id, USER_IMAGE_RAW_KEY, &new_raw).await;
    tracing::debug!("[user_image] appended raw fact ({} chars total)", new_raw.len());

    if new_raw.len() >= RAW_CONSOLIDATE_CHARS {
        consolidate_user_image(client, llm_url, db, account_id).await;
    }
}

/// Merge `user_image_raw` into `user_image` via a lightweight LLM call,
/// then clear raw. Called automatically when raw exceeds RAW_CONSOLIDATE_CHARS.
async fn consolidate_user_image(
    client: reqwest::Client,
    llm_url: String,
    db: SurrealDb,
    account_id: String,
) {
    let structured = load_setting(&db, &account_id, USER_IMAGE_KEY).await;
    let raw        = load_setting(&db, &account_id, USER_IMAGE_RAW_KEY).await;
    if raw.is_empty() { return; }

    let structured_section = if structured.is_empty() {
        "（尚無資料）".to_string()
    } else {
        structured.clone()
    };

    let field_list = PROFILE_FIELDS.join("、");
    let prompt = format!(
        "你是使用者畫像整合器。將以下「新增原文資訊」整合進「現有結構化畫像」，輸出更新後的完整畫像。\n\
        \n\
        輸出格式：每行一個欄位，格式為「欄位名：值」，優先欄位順序：{field_list}，其餘欄位依序排列。\n\
        \n\
        欄位說明：\n\
        - 所在地：居住城市或地區\n\
        - 語言：慣用語言，e.g. 繁體中文\n\
        - 回答風格：e.g. 簡潔條列、詳細說明、口語、正式\n\
        - 專業背景：技術能力與深度，e.g. Rust/React 熟練，ML 初學者\n\
        - 交通習慣：e.g. 捷運、開車、步行\n\
        - 時區：e.g. Asia/Taipei\n\
        \n\
        規則：\n\
        - 新資訊優先（若與現有矛盾，以新的為準）\n\
        - 只保留恆定個人屬性，不記錄具體事件或決策\n\
        - 只輸出欄位列表，不要任何說明\n\
        \n\
        ### 現有結構化畫像\n\
        {structured_section}\n\
        \n\
        ### 新增原文資訊\n\
        {raw}"
    );

    let body = json!({
        "messages": [{ "role": "user", "content": prompt }],
        "stream": false,
        "temperature": 0.0,
        "max_tokens": 200,
    });

    let short_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or(client);

    let url = format!("{}/v1/chat/completions", llm_url.trim_end_matches('/'));
    let resp = match short_client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("[user_image] consolidate LLM failed: {}", e);
            return;
        }
    };

    let result: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return,
    };

    let new_text = result["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    if new_text.is_empty() { return; }

    // Validate it looks like field lines (at least one `：`)
    if !new_text.contains('：') { return; }

    upsert_setting(&db, &account_id, USER_IMAGE_KEY, &new_text).await;
    upsert_setting(&db, &account_id, USER_IMAGE_RAW_KEY, "").await;

    tracing::info!(
        "[user_image] consolidated for account={} ({} chars)",
        account_id, new_text.len()
    );
}
