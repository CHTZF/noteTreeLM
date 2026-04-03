use serde_json::Value;
use crate::db::SurrealDb;
use crate::processing::embedder::cosine_sim;
use super::super::engine::context::vault_query_memory_with_limit;

pub(crate) struct SkillPassResult {
    pub system_injection: String,
    pub skill_titles: Vec<String>,
    pub skill_ids: Vec<String>,
    pub meta_functions: Vec<MetaFunctionSpec>,
}

/// A pre-planned tool chain exposed to LLM as a single callable function.
#[derive(Clone)]
pub(crate) struct MetaFunctionSpec {
    pub fn_name: String,
    pub description: String,
    pub chain: Vec<String>,
    pub fallback_msg: String,
    /// The skill_id this meta-function was derived from, for trigger_count tracking.
    pub skill_id: String,
}

fn sanitize_skill_fn_name(title: &str, skill_id: &str) -> String {
    let ascii: String = title.chars().map(|c| {
        if c.is_ascii_alphanumeric() { c } else { '_' }
    }).collect::<String>();
    let clean = ascii.trim_matches('_');
    if clean.len() < 2 {
        let safe_id: String = skill_id.chars().map(|c| if c == '-' { '_' } else { c }).collect();
        format!("skill_{}", &safe_id[..safe_id.len().min(12)])
    } else {
        format!("skill_{}", clean)
    }
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
            skill_ids: vec![],
            meta_functions: vec![],
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

    // Generate meta-functions for skills whose behavior contains @[tool] markers.
    let meta_functions: Vec<MetaFunctionSpec> = matched.iter()
        .filter(|(_, _, _, mode)| mode != "proactive")
        .filter_map(|(skill_id, title, behavior, _)| {
            let chain = extract_chain_from_behavior(behavior);
            if chain.is_empty() { return None; }
            Some(MetaFunctionSpec {
                fn_name: sanitize_skill_fn_name(title, skill_id),
                description: behavior.clone(),
                chain,
                fallback_msg: "找不到相關內容，請換個說法再試試。".to_string(),
                skill_id: skill_id.clone(),
            })
        })
        .collect();

    // Chain tool names (exposed via meta-functions, not given to LLM individually).
    let chain_tool_set: std::collections::HashSet<String> = meta_functions.iter()
        .flat_map(|m| m.chain.iter().cloned())
        .collect();

    // Non-chain skills (no @[tool] in behavior) → system injection text.
    let skill_text: String = matched.iter()
        .filter(|(_, _, behavior, mode)| {
            mode != "proactive" && extract_chain_from_behavior(behavior).is_empty() && !behavior.is_empty()
        })
        .map(|(_, title, beh, _)| format!("[技能：{}]\n{}", title, beh))
        .collect::<Vec<_>>().join("\n\n");

    let mut injection_parts: Vec<String> = vec![];
    let proactive_ctx = proactive_parts.join("\n\n");
    if !proactive_ctx.is_empty() { injection_parts.push(proactive_ctx.chars().take(1000).collect()); }
    if !skill_text.is_empty() {
        // Strip @[tool] markers from injected text for readability
        let clean = strip_tool_markers(&skill_text);
        injection_parts.push(clean.chars().take(1500).collect());
    }

    // Remove chain tools from the agent's direct tool access — they're covered by meta-functions.
    let _ = chain_tool_set; // currently no direct tool list from skills

    SkillPassResult {
        system_injection: injection_parts.join("\n\n"),
        skill_titles: matched.iter().map(|(_, t, _, _)| t.clone()).collect(),
        skill_ids: matched.iter().map(|(id, _, _, _)| id.clone()).collect(),
        meta_functions,
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
