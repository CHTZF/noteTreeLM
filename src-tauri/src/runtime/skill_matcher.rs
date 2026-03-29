/// runtime/skill_matcher.rs
///
/// 技能匹配相關函式：
///   detect_response_framework / search_skills_for_tool


/// 偵測回覆是否包含可重用的結構化回答框架（bottom-up skill 歸納觸發條件）
pub(crate) fn detect_response_framework(text: &str) -> bool {
    // 含有編號步驟（1. 2. 3. 或 ①②③）
    let has_numbered = (text.contains("1.") || text.contains("1、") || text.contains("①"))
        && (text.contains("2.") || text.contains("2、") || text.contains("②"));
    // 含有「先…再…最後」結構
    let has_sequential = (text.contains("先") && text.contains("再") && text.contains("最後"))
        || (text.contains("首先") && text.contains("接著"));
    // 含有明顯框架關鍵字
    let has_framework_kw = text.contains("步驟") || text.contains("流程") || text.contains("規範");
    // 回覆夠長（>300 字）才考慮
    text.len() > 300 && (has_numbered || has_sequential || has_framework_kw)
}

/// 工具用：根據 use_ask 語意搜尋最相似的技能規範（daemon 版）。
/// 回傳 Vec<(skill_id, title, behavior, tool_calls, need_tool_chain, tool_chain_order, injection_mode)>。
pub(crate) async fn search_skills_for_tool(
    http_client: &reqwest::Client,
    auth_token: &str,
    vault_id: &str,
    use_ask: &str,
    _emb_url: Option<&str>,
    _llama_client: &reqwest::Client,
) -> Vec<(String, String, String, Vec<String>, bool, Vec<String>, String)> {
    let tok = if auth_token.is_empty() { None } else { Some(auth_token) };
    // daemon 的 GET /vaults/:vid/skills 直接回傳 JSON array（非 {"skills":[...]}）
    let result: serde_json::Value = crate::api_client::daemon_get(
        http_client,
        &format!("/vaults/{}/skills", urlencoding::encode(vault_id)),
        tok,
    ).await.unwrap_or_else(|_| serde_json::json!([]));
    let skills = match result.as_array() {
        Some(arr) => arr.clone(),
        None => return vec![],
    };
    let use_ask_lower = use_ask.to_lowercase();
    skills.iter().filter(|s| {
        let is_active = s["is_active"].as_bool().unwrap_or(true);
        let trigger = s["trigger"].as_str().unwrap_or("").to_lowercase();
        let mode = s["injection_mode"].as_str().unwrap_or("passive");
        if !is_active { return false; }
        if mode == "active" || mode == "proactive" { return true; }
        trigger.split(['、', ',', '，']).any(|kw| {
            let kw = kw.trim();
            !kw.is_empty() && use_ask_lower.contains(kw)
        })
    }).map(|s| {
        let skill_id = s["skill_id"].as_str().unwrap_or("").to_string();
        let title = s["title"].as_str().unwrap_or("").to_string();
        let behavior = s["behavior"].as_str().unwrap_or("").to_string();
        // tool_calls 可能是 native array（新版 seed）或 JSON string（舊版 create_agent_skill）
        let tool_calls: Vec<String> = if let Some(arr) = s["tool_calls"].as_array() {
            arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
        } else if let Some(s) = s["tool_calls"].as_str() {
            serde_json::from_str::<Vec<String>>(s).unwrap_or_default()
        } else {
            vec![]
        };
        let need_tool_chain = s["need_tool_chain"].as_bool().unwrap_or(false);
        let tool_chain_order: Vec<String> = if let Some(arr) = s["tool_chain_order"].as_array() {
            arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
        } else {
            vec![]
        };
        let injection_mode = s["injection_mode"].as_str().unwrap_or("passive").to_string();
        (skill_id, title, behavior, tool_calls, need_tool_chain, tool_chain_order, injection_mode)
    }).collect()
}

