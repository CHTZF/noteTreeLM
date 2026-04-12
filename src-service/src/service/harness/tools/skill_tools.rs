use serde_json::Value;
use crate::db::SurrealDb;
use crate::embedding::embedder::cosine_sim;
use crate::service::harness::memory::semantic::vault_query_memory_with_limit;

#[derive(Clone)]
pub(crate) struct SkillPassResult {
    pub system_injection: String,
    pub skill_titles: Vec<String>,
    /// Tools required by matched skills' chains — caller should add these to tool_names.
    pub skill_tool_names: Vec<String>,
}

/// Extract ordered tool names from `@[tool_name]` markers in behavior text.
/// Preserves first-occurrence order; deduplicates.
pub(crate) fn extract_chain_from_behavior(behavior: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut rest = behavior;
    while let Some(start) = rest.find("@[") {
        rest = &rest[start + 2..];
        if let Some(end) = rest.find(']') {
            let name = rest[..end].trim().to_string();
            if !name.is_empty() && seen.insert(name.clone()) {
                result.push(name);
            }
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    result
}

// (skill_id, title, behavior, injection_mode)
type SkillRow = (String, String, String, String);

/// Keyword fallback: filter active skills by trigger keywords in input.
pub(crate) async fn search_skills_db(
    db: &SurrealDb,
    account_id: &str,
    input: &str,
) -> Vec<SkillRow> {
    let mut resp = match db
        .query("SELECT *, record::id(id) AS id FROM agent_skills WHERE account_id = $aid AND is_active = true")
        .bind(("aid", account_id.to_string()))
        .await
    {
        Ok(r) => r,
        Err(_) => return vec![],
    };
    let skills: Vec<Value> = resp.take(0).unwrap_or_default();
    let input_lower = input.to_lowercase();
    skills.iter().filter(|s| {
        let trigger = s["trigger"].as_str().unwrap_or("").to_lowercase();
        let mode = s["injection_mode"].as_str().unwrap_or("passive");
        if mode == "active" || mode == "proactive" { return true; }
        trigger.split(['、', ',', '，']).any(|kw| {
            let kw = kw.trim().to_lowercase();
            !kw.is_empty() && input_lower.contains(kw.as_str())
        })
    }).map(skill_row_to_tuple).collect()
}

fn skill_row_to_tuple(s: &Value) -> SkillRow {
    let skill_id      = s["skill_id"].as_str().unwrap_or("").to_string();
    let title         = s["title"].as_str().unwrap_or("").to_string();
    let behavior      = s["behavior"].as_str().unwrap_or("").to_string();
    let injection_mode = s["injection_mode"].as_str().unwrap_or("passive").to_string();
    (skill_id, title, behavior, injection_mode)
}

/// Semantic search: embeds input, scores skills via cosine, threshold 0.65.
async fn semantic_skill_search(
    client: &reqwest::Client,
    embedding_url: &Option<String>,
    db: &SurrealDb,
    account_id: &str,
    input: &str,
    threshold: f32,
) -> Option<Vec<SkillRow>> {
    let input_vec = crate::embedding::embedder::embed_text(client, embedding_url, input).await?;
    if input_vec.is_empty() { return None; }

    let mut resp = db
        .query("SELECT *, record::id(id) AS id FROM agent_skills WHERE account_id = $aid AND is_active = true AND embedding IS NOT NONE")
        .bind(("aid", account_id.to_string()))
        .await.ok()?;
    let skills: Vec<Value> = resp.take(0).ok()?;
    if skills.is_empty() { return None; }

    let mut all_scores: Vec<(f32, &Value)> = skills.iter().filter_map(|s| {
        let emb: Vec<f32> = s["embedding"].as_array()?
            .iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
        if emb.is_empty() { return None; }
        let mode = s["injection_mode"].as_str().unwrap_or("passive");
        if mode == "active" || mode == "proactive" { return Some((1.0f32, s)); }
        Some((cosine_sim(&input_vec, &emb), s))
    }).collect();

    all_scores.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Log top-3 scores for debugging skill matching
    for (score, s) in all_scores.iter().take(3) {
        tracing::info!("[skill_pass] top score={:.3} title={:?}", score, s["title"].as_str().unwrap_or("?"));
    }

    let scored: Vec<(f32, &Value)> = all_scores.into_iter().filter(|(score, _)| *score >= threshold).collect();
    if scored.is_empty() { return None; }
    Some(scored.into_iter().map(|(_, s)| skill_row_to_tuple(s)).collect())
}

/// Run skill matching: semantic search first, keyword fallback if unavailable.
pub(crate) async fn run_skill_pass(
    client: &reqwest::Client,
    embedding_url: &Option<String>,
    db: &SurrealDb,
    vault_id: &str,
    account_id: &str,
    input: &str,
) -> SkillPassResult {
    let matched = match semantic_skill_search(client, embedding_url, db, account_id, input, 0.65).await {
        Some(r) if !r.is_empty() => r,
        _ => search_skills_db(db, account_id, input).await,
    };

    if matched.is_empty() {
        return SkillPassResult {
            system_injection: String::new(),
            skill_titles: vec![],
            skill_tool_names: vec![],
        };
    }

    // Proactive memory prefetch: skill with @[prefetch_memory] and injection_mode=proactive
    let mut proactive_parts: Vec<String> = vec![];
    for (_, _, behavior, mode) in &matched {
        if mode != "proactive" { continue; }
        let chain = extract_chain_from_behavior(behavior);
        if chain.iter().any(|t| t == "prefetch_memory") {
            let kw: String = input.chars().take(120).collect();
            let keywords = if kw.is_empty() { vec![] } else { vec![kw] };
            let facts = vault_query_memory_with_limit(client, embedding_url, db, vault_id, account_id, &keywords, 8).await;
            if !facts.is_empty() {
                let lines: Vec<String> = facts.iter().map(|r| {
                    format!("[{}] {}", r["category"].as_str().unwrap_or("general"), r["content"].as_str().unwrap_or(""))
                }).collect();
                proactive_parts.push(format!("## 相關記憶\n{}", lines.join("\n")));
            }
        }
    }

    // Collect tool names and injection text from all non-proactive skills.
    let mut chain_tool_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut injection_parts: Vec<String> = vec![];

    let proactive_ctx = proactive_parts.join("\n\n");
    if !proactive_ctx.is_empty() { injection_parts.push(proactive_ctx.chars().take(1000).collect()); }

    for (_, title, behavior, mode) in &matched {
        if mode == "proactive" { continue; }
        let chain = extract_chain_from_behavior(behavior);
        chain.iter().for_each(|t| { chain_tool_set.insert(t.clone()); });
        if !behavior.is_empty() {
            let clean = strip_tool_markers(behavior);
            injection_parts.push(format!("[技能：{}]\n{}", title, clean).chars().take(1500).collect());
        }
    }

    let skill_tool_names: Vec<String> = chain_tool_set.into_iter().collect();

    SkillPassResult {
        system_injection: injection_parts.join("\n\n").chars().take(2000).collect(),
        skill_titles: matched.iter().map(|(_, t, _, _)| t.clone()).collect(),
        skill_tool_names,
    }
}

/// Remove `@[tool_name]` markers from text for clean display/injection.
pub(crate) fn strip_tool_markers(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("@[") {
        result.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        if let Some(end) = rest.find(']') {
            // Replace @[tool_name] with just tool_name
            result.push_str(&rest[..end]);
            rest = &rest[end + 1..];
        }
    }
    result.push_str(rest);
    result
}

/// Load skills directly by their skill_ids, bypassing semantic/keyword matching.
/// Used when agent_def has `use_skill_pass: false` and `skill_ids` is non-empty.
pub(crate) async fn load_skills_by_ids(db: &SurrealDb, skill_ids: &[String]) -> SkillPassResult {
    if skill_ids.is_empty() {
        return SkillPassResult { system_injection: String::new(), skill_titles: vec![], skill_tool_names: vec![] };
    }
    let ids_json = serde_json::Value::Array(
        skill_ids.iter().map(|s| serde_json::Value::String(s.clone())).collect()
    );
    let mut resp = match db
        .query("SELECT skill_id, title, behavior, injection_mode FROM agent_skills WHERE skill_id INSIDE $ids AND is_active = true")
        .bind(("ids", ids_json))
        .await
    {
        Ok(r) => r,
        Err(_) => return SkillPassResult { system_injection: String::new(), skill_titles: vec![], skill_tool_names: vec![] },
    };
    let skills: Vec<serde_json::Value> = resp.take(0).unwrap_or_default();
    let rows: Vec<SkillRow> = skills.iter().map(skill_row_to_tuple).collect();
    if rows.is_empty() {
        return SkillPassResult { system_injection: String::new(), skill_titles: vec![], skill_tool_names: vec![] };
    }

    let mut chain_tool_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut injection_parts: Vec<String> = Vec::new();

    for (_, title, behavior, _) in &rows {
        let chain = extract_chain_from_behavior(behavior);
        chain.iter().for_each(|t| { chain_tool_set.insert(t.clone()); });
        if !behavior.is_empty() {
            let clean = strip_tool_markers(behavior);
            injection_parts.push(format!("[技能：{}]\n{}", title, clean).chars().take(1500).collect());
        }
    }

    SkillPassResult {
        system_injection: injection_parts.join("\n\n").chars().take(2000).collect(),
        skill_titles: rows.iter().map(|(_, t, _, _)| t.clone()).collect(),
        skill_tool_names: chain_tool_set.into_iter().collect(),
    }
}

/// When use_skill_pass is true but no skill matched the user's input,
/// inject a skill-discovery prompt so LLM can guide the user to select an
/// existing skill or compose a new one with @[tool_name] chain syntax.
pub(crate) async fn build_skill_discovery_injection(db: &SurrealDb, account_id: &str) -> String {
    let existing_skills: Vec<String> = {
        let resp = db
            .query("SELECT title, trigger FROM agent_skills WHERE account_id = $aid AND is_active = true ORDER BY trigger_count DESC LIMIT 20")
            .bind(("aid", account_id.to_string()))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<serde_json::Value>>(0).ok())
            .unwrap_or_default();
        resp.iter().map(|s| {
            let title = s["title"].as_str().unwrap_or("").to_string();
            let trigger = s["trigger"].as_str().unwrap_or("").to_string();
            format!("- {title}（觸發詞：{trigger}）")
        }).collect()
    };

    let available_tools = [
        "@[search_vault]      — 語意搜尋筆記",
        "@[list_structure]    — 列出資料夾結構",
        "@[read_note]         — 讀取指定筆記",
        "@[open_note]         — 開啟筆記（顯示於 UI）",
        "@[create_note]       — 建立新筆記",
        "@[update_note]       — 更新筆記內容",
        "@[append_to_note]    — 附加內容到筆記",
        "@[delete_note]       — 刪除筆記",
        "@[update_note_frontmatter] — 更新 frontmatter 欄位",
        "@[plan_announce]     — 宣告計畫並等待使用者確認後繼續",
        "@[web_search]        — 網路搜尋",
        "@[prefetch_memory]   — 預先載入相關記憶（proactive 模式專用）",
    ];

    let skills_section = if existing_skills.is_empty() {
        "（目前尚無已建立的技能）".to_string()
    } else {
        existing_skills.join("\n")
    };

    format!(
        "## 技能未命中 — 技能探索模式\n\
        使用者的輸入未觸發任何已知技能。請先與使用者確認意圖，\
        引導他選擇現有技能或描述新需求，然後使用 create_agent_skill 工具建立技能。\n\n\
        ### 建立技能的格式規則\n\
        - `behavior` 欄位用自然語言描述行為；如需工具鏈，用 @[tool_name] 標記工具呼叫順序\n\
        - 範例：「先用 @[search_vault] 找到筆記，再用 @[plan_announce] 確認，最後 @[update_note] 更新內容」\n\
        - `injection_mode` 可選 `passive`（依關鍵字觸發）/ `active`（每次都觸發）/ `proactive`（自動背景預載）\n\
        - `trigger` 填寫觸發關鍵詞，多個關鍵詞以逗號分隔\n\n\
        ### 可用工具（@[tool_name] 語法）\n\
        {}\n\n\
        ### 目前已建立的技能\n\
        {}",
        available_tools.join("\n"),
        skills_section,
    )
}
