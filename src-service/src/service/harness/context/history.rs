use serde_json::{json, Value};
use super::super::prompt::{templates, Locale};

/// Summarise oldest turns when history exceeds `history_chars` budget.
/// Keeps the `keep_recent` most recent turns verbatim; older turns are
/// replaced with an LLM-generated summary.
/// Returns `(trimmed_history, was_trimmed)`.
pub(super) async fn trim_history(
    hist:          Vec<Value>,
    history_chars: usize,
    keep_recent:   usize,
    client:        &reqwest::Client,
    llm_url:       &str,
    locale:        Locale,
) -> (Vec<Value>, bool) {
    let total: usize = hist.iter()
        .map(|m| m["content"].as_str().unwrap_or("").len())
        .sum();

    if total <= history_chars || hist.len() <= keep_recent {
        return (hist, false);
    }

    let keep_from = hist.len().saturating_sub(keep_recent);
    let old_text: String = hist[..keep_from].iter().map(|m| {
        let role    = m["role"].as_str().unwrap_or("user");
        let content = m["content"].as_str().unwrap_or("");
        format!("[{}]: {}", role, &content[..content.len().min(500)])
    }).collect::<Vec<_>>().join("\n\n");
    let recent = hist[keep_from..].to_vec();

    let summary = summarise_with_llm(client, llm_url, &old_text, locale).await;
    let trimmed = if summary.is_empty() {
        recent
    } else {
        let mut v = vec![json!({"role": "assistant", "content": templates::history_summary_msg(&summary, locale)})];
        v.extend(recent);
        v
    };
    (trimmed, true)
}

async fn summarise_with_llm(
    client:   &reqwest::Client,
    llm_url:  &str,
    old_text: &str,
    locale:   Locale,
) -> String {
    let result: Option<String> = async {
        let resp = client
            .post(format!("{}/v1/chat/completions", llm_url))
            .json(&json!({
                "messages": [
                    {"role": "system", "content": templates::history_summarise_system(locale)},
                    {"role": "user",   "content": old_text},
                ],
                "stream": false,
                "temperature": 0.1,
                "max_tokens": 400,
            }))
            .send().await.ok()?;
        if !resp.status().is_success() { return None; }
        let j: serde_json::Value = resp.json().await.ok()?;
        j["choices"][0]["message"]["content"].as_str().map(String::from)
    }.await;
    result.unwrap_or_default()
}
