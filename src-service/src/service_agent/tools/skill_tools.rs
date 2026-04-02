use serde_json::Value;
use crate::db::SurrealDb;
use crate::processing::embedder::cosine_sim;
use super::super::engine::context::vault_query_memory_with_limit;

pub(crate) struct SkillPassResult {
    pub tool_names: Vec<String>,
    pub system_injection: String,
    pub skill_titles: Vec<String>,
    pub skill_ids: Vec<String>,
    pub meta_functions: Vec<MetaFunctionSpec>,
}

/// A pre-planned tool chain exposed to LLM as a single callable function.
/// LLM provides all possible params upfront; execution starts from the last
/// tool whose params are satisfied and falls back to earlier tools as needed.
#[derive(Clone)]
pub(crate) struct MetaFunctionSpec {
    pub fn_name: String,
    pub description: String,
    pub chain: Vec<String>,
    pub fallback_msg: String,
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

/// Keyword fallback: filter active skills by trigger keywords in input.
pub(crate) async fn search_skills_db(
    db: &SurrealDb,
    account_id: &str,
    input: &str,
) -> Vec<(String, String, String, Vec<String>, bool, Vec<String>, String)> {
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

fn skill_row_to_tuple(s: &Value) -> (String, String, String, Vec<String>, bool, Vec<String>, String) {
    let skill_id = s["skill_id"].as_str().unwrap_or("").to_string();
    let title = s["title"].as_str().unwrap_or("").to_string();
    let behavior = s["behavior"].as_str().unwrap_or("").to_string();
    let tool_calls: Vec<String> = if let Some(arr) = s["tool_calls"].as_array() {
        arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
    } else if let Some(s) = s["tool_calls"].as_str() {
        serde_json::from_str::<Vec<String>>(s).unwrap_or_default()
    } else { vec![] };
    let need_tool_chain = s["need_tool_chain"].as_bool().unwrap_or(false);
    let tool_chain_order: Vec<String> = if let Some(arr) = s["tool_chain_order"].as_array() {
        arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
    } else { vec![] };
    let injection_mode = s["injection_mode"].as_str().unwrap_or("passive").to_string();
    (skill_id, title, behavior, tool_calls, need_tool_chain, tool_chain_order, injection_mode)
}

/// Semantic search: embeds input, scores skills with embeddings via cosine, threshold 0.65.
/// Returns None if embedding unavailable or no skills pass threshold.
async fn semantic_skill_search(
    client: &reqwest::Client,
    embedding_url: &Option<String>,
    db: &SurrealDb,
    account_id: &str,
    input: &str,
    threshold: f32,
) -> Option<Vec<(String, String, String, Vec<String>, bool, Vec<String>, String)>> {
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
            tool_names: vec![],
            system_injection: String::new(),
            skill_titles: vec![],
            skill_ids: vec![],
            meta_functions: vec![],
        };
    }

    // Proactive memory prefetch
    let mut proactive_parts: Vec<String> = vec![];
    for (_, _, _, _, need_chain, chain_order, mode) in &matched {
        if mode != "proactive" || !*need_chain { continue; }
        for tool_name in chain_order {
            if tool_name == "prefetch_memory" {
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
    }

    // Generate meta-functions for skills with tool_chain_order.
    // These become single callable functions for LLM instead of individual tools.
    let meta_functions: Vec<MetaFunctionSpec> = matched.iter()
        .filter(|(_, _, _, _, need_chain, chain_order, _)| *need_chain && !chain_order.is_empty())
        .map(|(skill_id, title, behavior, _, _, chain_order, _)| MetaFunctionSpec {
            fn_name: sanitize_skill_fn_name(title, skill_id),
            description: behavior.clone(),
            chain: chain_order.clone(),
            fallback_msg: "找不到相關內容，請換個說法再試試。".to_string(),
        })
        .collect();

    // Chain tools are exposed via meta-functions; exclude them from direct tool list.
    let chain_tool_set: std::collections::HashSet<String> = meta_functions.iter()
        .flat_map(|m| m.chain.iter().cloned())
        .collect();

    // Non-chain skills go into skill_text (system injection).
    let skill_text: String = matched.iter()
        .filter(|(_, _, beh, _, need_chain, chain_order, mode)| {
            !beh.is_empty() && mode != "proactive" && !(*need_chain && !chain_order.is_empty())
        })
        .map(|(_, title, beh, _, _, _, _)| format!("[技能：{}]\n{}", title, beh))
        .collect::<Vec<_>>().join("\n\n");

    let mut injection_parts: Vec<String> = vec![];
    let proactive_ctx = proactive_parts.join("\n\n");
    if !proactive_ctx.is_empty() { injection_parts.push(proactive_ctx.chars().take(1000).collect()); }
    if !skill_text.is_empty() { injection_parts.push(skill_text.chars().take(1500).collect()); }

    let mut tool_names: Vec<String> = matched.iter()
        .flat_map(|(_, _, _, tc, need_chain, chain_order, _)| {
            if *need_chain && !chain_order.is_empty() { vec![] } else { tc.clone() }
        })
        .filter(|t| !chain_tool_set.contains(t))
        .collect::<std::collections::HashSet<_>>()
        .into_iter().collect();
    tool_names.sort();

    SkillPassResult {
        tool_names,
        system_injection: injection_parts.join("\n\n"),
        skill_titles: matched.iter().map(|(_, t, _, _, _, _, _)| t.clone()).collect(),
        skill_ids: matched.iter().map(|(id, _, _, _, _, _, _)| id.clone()).collect(),
        meta_functions,
    }
}
