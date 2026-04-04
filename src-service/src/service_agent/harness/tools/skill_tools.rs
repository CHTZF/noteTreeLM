use serde_json::Value;
use crate::db::SurrealDb;
use crate::processing::embedder::cosine_sim;
use crate::service_agent::engine::context::vault_query_memory_with_limit;

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
            let kw = kw.trim();
            !kw.is_empty() && input_lower.contains(kw)
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
    let input_vec = crate::processing::embedder::embed_text(client, embedding_url, input).await?;
    if input_vec.is_empty() { return None; }

    let mut resp = db
        .query("SELECT *, record::id(id) AS id FROM agent_skills WHERE account_id = $aid AND is_active = true AND embedding IS NOT NONE")
        .bind(("aid", account_id.to_string()))
        .await.ok()?;
    let skills: Vec<Value> = resp.take(0).ok()?;
    if skills.is_empty() { return None; }

    let mut scored: Vec<(f32, &Value)> = skills.iter().filter_map(|s| {
        let emb: Vec<f32> = s["embedding"].as_array()?
            .iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
        if emb.is_empty() { return None; }
        let mode = s["injection_mode"].as_str().unwrap_or("passive");
        if mode == "active" || mode == "proactive" { return Some((1.0f32, s)); }
        let score = cosine_sim(&input_vec, &emb);
        if score >= threshold { Some((score, s)) } else { None }
    }).collect();

    if scored.is_empty() { return None; }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
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
        if chain.is_empty() {
            // No tool markers — inject raw behavior text as guidance.
            if !behavior.is_empty() {
                let clean = strip_tool_markers(behavior);
                injection_parts.push(format!("[技能：{}]\n{}", title, clean).chars().take(500).collect());
            }
        } else {
            // Chain skill: inject as ordered operation hint for LLM, expose tools via skill_tool_names.
            chain.iter().for_each(|t| { chain_tool_set.insert(t.clone()); });
            let step_hint = chain.iter()
                .filter(|t| t.as_str() != "plan_announce")
                .cloned()
                .collect::<Vec<_>>()
                .join(" → ");
            if !step_hint.is_empty() {
                let clean_desc = strip_tool_markers(behavior);
                injection_parts.push(
                    format!("[技能：{}]\n建議操作順序：{}\n{}", title, step_hint, clean_desc)
                        .chars().take(600).collect()
                );
            }
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
