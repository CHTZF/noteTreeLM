/// runtime/agent_factory.rs
///
/// Agent 規格生成：
///   generate_agent_spec / extract_cjk_keywords

use std::time::Duration;

/// 呼叫 LLM（非串流）根據 user_ask 生成 agent 規格 JSON。
/// 回傳 (name, description, trigger, tool_names, system_prompt, skills)；任何錯誤 fallback 至 raw input。
#[allow(dead_code)]
pub(crate) async fn generate_agent_spec(
    client: &reqwest::Client,
    base_url: &str,
    input: &str,
) -> (String, String, String, Vec<String>, String, Vec<crate::runtime::system_agent::NewSkillSpec>) {
    let fallback = || (
        input.chars().take(24).collect::<String>(),
        input.to_string(),
        input.to_string(),
        vec![],
        String::new(),
        vec![],
    );

    let system = "\
你是一個 agent 規劃助理。根據使用者需求，輸出 JSON agent 規格（只輸出 JSON，不加任何說明）。\n\
格式：\n\
{\n\
  \"name\": \"<10字以內的中文名稱>\",\n\
  \"description\": \"<此agent專門做什麼>\",\n\
  \"trigger\": \"<何時觸發此agent的語意描述>\",\n\
  \"tool_names\": [<工具列表>],\n\
  \"system_prompt\": \"<此agent的繁體中文任務指令，說明如何解讀使用者語意、工具使用順序，2-4句話>\",\n\
  \"skills\": [\n\
    {\n\
      \"title\": \"<技能名稱>\",\n\
      \"trigger\": \"<何時套用此技能的語意描述>\",\n\
      \"behavior\": \"<具體行為規範，說明遇到此情境應如何處理>\",\n\
      \"injection_mode\": \"passive\"\n\
    }\n\
  ]\n\
}\n\
\n\
可用 tool_names：search_vault, read_note, open_note, list_structure, create_note, update_note, create_folder, query_memory, web_search, list_recent_conversations\n\
選擇原則：\n\
- 筆記查詢/搜尋 → [\"search_vault\"]\n\
- 筆記打開（讓使用者在編輯器中查看）→ [\"search_vault\",\"open_note\"]\n\
- 筆記閱讀/分析內容 → [\"search_vault\",\"read_note\"]\n\
- 筆記寫入/更新 → [\"create_note\",\"update_note\",\"create_folder\"]\n\
- 外部資訊/網路查詢 → [\"web_search\"]\n\
- 記憶查詢 → [\"query_memory\"]\n\
- 複合任務 → 組合上述\n\
\n\
skills 撰寫原則：\n\
- 每個 skill 對應一種使用者可能的語意變體或邊緣情境（例如：使用者說「找不到」時的 fallback 行為）\n\
- 0-3 個 skills，只有確實需要時才加（簡單任務可以 skills: []）\n\
- behavior 要具體可執行，不要空泛\n\
\n\
system_prompt 撰寫重點：說明使用者的用詞習慣、意圖語意、工具使用順序（例如：先 search_vault 再 open_note），禁止虛構結果。";

    let body = serde_json::json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": input},
        ],
        "max_tokens": 256,
        "temperature": 0.3,
        "stream": false,
    });

    let resp = match client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return fallback(),
    };

    let json: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return fallback(),
    };

    let text = json["choices"][0]["message"]["content"].as_str().unwrap_or("");
    let start = match text.find('{') { Some(i) => i, None => return fallback() };
    let end   = match text.rfind('}') { Some(i) => i + 1, None => return fallback() };

    let spec: serde_json::Value = match serde_json::from_str(&text[start..end]) {
        Ok(v) => v,
        Err(_) => return fallback(),
    };

    let name = spec["name"].as_str().unwrap_or("").chars().take(24).collect::<String>();
    let desc = spec["description"].as_str().unwrap_or(input).to_string();
    let trigger = spec["trigger"].as_str().unwrap_or(input).to_string();
    let tools: Vec<String> = spec["tool_names"].as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let system_prompt = spec["system_prompt"].as_str().unwrap_or("").to_string();
    let skills: Vec<crate::runtime::system_agent::NewSkillSpec> = spec["skills"].as_array()
        .map(|arr| arr.iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect())
        .unwrap_or_default();

    let name = if name.is_empty() { input.chars().take(24).collect() } else { name };
    (name, desc, trigger, tools, system_prompt, skills)
}

/// 從查詢文字取出最多 N 個有意義的 CJK bigram，供 keyword 搜尋
#[allow(dead_code)]
pub(crate) fn extract_cjk_keywords(text: &str, max: usize) -> Vec<String> {
    const STOPS: &[char] = &[
        '你','我','他','她','它','的','了','嗎','是','有','在','說','道','記',
        '什','麼','這','那','就','都','也','還','不','沒','要','會','可','以',
        '和','與','或','但','如','果','因','為','所','而','且','呢','嗎','啊',
    ];
    let cjk: Vec<char> = text.chars()
        .filter(|c| *c as u32 >= 0x4E00 && *c as u32 <= 0x9FFF)
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for pair in cjk.windows(2) {
        if STOPS.contains(&pair[0]) && STOPS.contains(&pair[1]) { continue; }
        let bigram: String = pair.iter().collect();
        if seen.insert(bigram.clone()) {
            out.push(bigram);
            if out.len() >= max { break; }
        }
    }
    out
}
