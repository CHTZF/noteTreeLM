use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::service_agent::harness::observability::emitter::ObservabilityEmitter;
use crate::service_agent::harness::memory::working::WorkingMemory;

/// Returns the byte offset up to which `s` can safely be emitted without risking
/// a partial prefix of `tag` at the end.
/// Works on raw bytes — `tag` must be pure ASCII (guaranteed for `<tool_call>`).
fn safe_emit_end(s: &str, tag: &str) -> usize {
    let s_bytes = s.as_bytes();
    let tag_bytes = tag.as_bytes();
    let n = s_bytes.len();
    for suffix_len in 1..=tag_bytes.len().min(n) {
        let suffix = &s_bytes[n - suffix_len..];
        if tag_bytes.starts_with(suffix) {
            // The returned offset is right before ASCII bytes → always a valid char boundary.
            return n - suffix_len;
        }
    }
    n
}

/// Validate citation ids from [cite:id1,id2] against the working memory evidence store.
/// Returns true if all ids are known (or working_memory is None = no validation needed).
async fn validate_citation(
    cite_inner: &str,
    working_memory: Option<&WorkingMemory>,
) -> bool {
    let wm = match working_memory {
        Some(w) => w,
        None => return true,
    };
    if cite_inner.trim() == "none" { return true; }
    let ids: Vec<String> = cite_inner.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if ids.is_empty() { return true; }
    wm.with_records(|map| ids.iter().all(|id| map.contains_key(id.as_str()))).await
}

/// Stream one LLM round, emitting llm:token events. Returns (text, finish_reason, tool_chunks).
/// tool_chunks: Vec<(id, name, arguments_str)>
pub(crate) async fn stream_llm_round(
    client: &reqwest::Client,
    llm_url: &str,
    body: Value,
    emitter: &ObservabilityEmitter,
    cancel: &Arc<AtomicBool>,
    working_memory: Option<&WorkingMemory>,
) -> Result<(String, String, Vec<(String, String, String)>), String> {
    let resp = client
        .post(format!("{}/v1/chat/completions", llm_url))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("llm error {}: {}", status, text));
    }

    let mut stream = resp.bytes_stream();
    let mut sse_buf = String::new();
    let mut full_text = String::new();
    let mut finish_reason = "stop".to_string();
    let mut tool_chunks: Vec<(String, String, String)> = Vec::new();

    // Lookahead buffer: holds content not yet emitted, to avoid streaming partial <tool_call> tags
    let mut pending_emit = String::new();
    let mut suppress_emit = false;  // true once we've detected a text-format <tool_call>

    // Citation interceptor state machine.
    // Buffers up to CITE_BUFFER_LIMIT chars to detect [cite:...] at start of response.
    const CITE_BUFFER_LIMIT: usize = 60;
    enum CiteState { Buffering, Forwarding }
    let mut cite_state = CiteState::Buffering;
    let mut cite_buf = String::new();
    let mut cite_retry_done = false;

    while let Some(item) = stream.next().await {
        if cancel.load(Ordering::Relaxed) { break; }
        let bytes = item.map_err(|e| e.to_string())?;
        sse_buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(event_end) = sse_buf.find("\n\n") {
            let event = sse_buf[..event_end].to_string();
            sse_buf = sse_buf[event_end + 2..].to_string();

            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" { continue; }
                    if let Ok(j) = serde_json::from_str::<Value>(data) {
                        let choice = &j["choices"][0];
                        if let Some(fr) = choice["finish_reason"].as_str() {
                            if !fr.is_empty() { finish_reason = fr.to_string(); }
                        }
                        let delta = &choice["delta"];
                        if let Some(content) = delta["content"].as_str() {
                            if !content.is_empty() {
                                full_text.push_str(content);
                                if !suppress_emit {
                                    // ── Citation interceptor ──────────────────
                                    let emit_content = match cite_state {
                                        CiteState::Forwarding => {
                                            Some(content.to_string())
                                        }
                                        CiteState::Buffering => {
                                            cite_buf.push_str(content);
                                            // Try to parse [cite:...] from buffer
                                            if let Some(cite_end) = cite_buf.find(']') {
                                                if cite_buf.starts_with("[cite:") {
                                                    let cite_inner = &cite_buf[6..cite_end]; // between [cite: and ]
                                                    let valid = validate_citation(cite_inner, working_memory).await;
                                                    if !valid {
                                                        tracing::warn!(
                                                            "[citation] invalid cite ids: [cite:{}]",
                                                            cite_inner
                                                        );
                                                        emitter.emit("agent:citation_missing".to_string(), json!({}));
                                                    }
                                                    // Strip [cite:...] from what we forward
                                                    let rest = cite_buf[cite_end + 1..].to_string();
                                                    cite_buf.clear();
                                                    cite_state = CiteState::Forwarding;
                                                    if rest.is_empty() { None } else { Some(rest) }
                                                } else {
                                                    // ] found but doesn't start with [cite: — forward buffer
                                                    let flushed = cite_buf.clone();
                                                    cite_buf.clear();
                                                    cite_state = CiteState::Forwarding;
                                                    Some(flushed)
                                                }
                                            } else if cite_buf.len() >= CITE_BUFFER_LIMIT {
                                                // Timeout: no [cite:...] found — treat as [cite:none]
                                                if !cite_retry_done && working_memory.is_some() {
                                                    // Emit warning event; forward buffered content
                                                    emitter.emit("agent:citation_missing".to_string(), json!({}));
                                                    cite_retry_done = true;
                                                }
                                                let flushed = cite_buf.clone();
                                                cite_buf.clear();
                                                cite_state = CiteState::Forwarding;
                                                Some(flushed)
                                            } else {
                                                None // still buffering
                                            }
                                        }
                                    };
                                    if let Some(emit_str) = emit_content {
                                        pending_emit.push_str(&emit_str);
                                        if pending_emit.contains("<tool_call>") {
                                            let pos = pending_emit.find("<tool_call>").unwrap_or(0);
                                            if pos > 0 {
                                                emitter.emit("llm:token".to_string(), json!(&pending_emit[..pos]));
                                            }
                                            pending_emit.clear();
                                            suppress_emit = true;
                                        } else {
                                            let safe_end = safe_emit_end(&pending_emit, "<tool_call>");
                                            if safe_end > 0 {
                                                let chunk = pending_emit[..safe_end].to_string();
                                                pending_emit = pending_emit[safe_end..].to_string();
                                                emitter.emit("llm:token".to_string(), json!(chunk));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(tc_arr) = delta["tool_calls"].as_array() {
                            for tc in tc_arr {
                                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                                while tool_chunks.len() <= idx {
                                    tool_chunks.push((String::new(), String::new(), String::new()));
                                }
                                let acc = &mut tool_chunks[idx];
                                if let Some(id) = tc["id"].as_str() { if !id.is_empty() { acc.0 = id.to_string(); } }
                                if let Some(n) = tc["function"]["name"].as_str() { if !n.is_empty() { acc.1 = n.to_string(); } }
                                if let Some(a) = tc["function"]["arguments"].as_str() { acc.2.push_str(a); }
                            }
                        }
                    }
                }
            }
        }
    }

    // Flush remaining citation buffer (stream ended before we saw [cite:...])
    if !cite_buf.is_empty() {
        pending_emit.push_str(&cite_buf);
        cite_buf.clear();
    }
    // Flush remaining pending (if stream ended without <tool_call>)
    if !suppress_emit && !pending_emit.is_empty() {
        emitter.emit("llm:token".to_string(), json!(pending_emit));
    }

    // Parse text-format tool calls from full_text (e.g. Qwen/Mistral <tool_call> style)
    // and strip them from the display text
    if tool_chunks.is_empty() && full_text.contains("<tool_call>") {
        let mut clean_text = String::new();
        let mut rest = full_text.as_str();
        let mut tc_idx = 0usize;
        while let Some(start) = rest.find("<tool_call>") {
            clean_text.push_str(&rest[..start]);
            let after_open = &rest[start + "<tool_call>".len()..];
            if let Some(end) = after_open.find("</tool_call>") {
                let json_str = after_open[..end].trim();
                if let Ok(tc) = serde_json::from_str::<Value>(json_str) {
                    let name = tc["name"].as_str().unwrap_or("").to_string();
                    let args = tc["arguments"].clone();
                    let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
                    if !name.is_empty() {
                        tool_chunks.push((format!("tc_text_{}", tc_idx), name, args_str));
                        tc_idx += 1;
                    }
                }
                rest = &after_open[end + "</tool_call>".len()..];
            } else {
                // Incomplete tag — keep remainder as-is
                clean_text.push_str("<tool_call>");
                clean_text.push_str(after_open);
                rest = "";
                break;
            }
        }
        clean_text.push_str(rest);
        full_text = clean_text.trim().to_string();
    }

    Ok((full_text, finish_reason, tool_chunks))
}

/// Non-streaming LLM call for sub-agents. Returns (content, tool_chunks).
/// Does NOT emit llm:token events — caller handles output.
pub(crate) async fn call_llm_once(
    client: &reqwest::Client,
    llm_url: &str,
    messages: &[Value],
    tools: Option<Value>,
    cancel: &Arc<AtomicBool>,
) -> Result<(String, Vec<(String, String, String)>), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let mut body = json!({
        "messages": messages,
        "stream": false,
        "temperature": 0.7,
        "max_tokens": 2048,
    });
    if let Some(t) = tools {
        body["tools"] = t;
        body["tool_choice"] = json!("auto");
    }
    let resp = client
        .post(format!("{}/v1/chat/completions", llm_url))
        .json(&body)
        .send().await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("llm error {}: {}", status, text));
    }
    let j: Value = resp.json().await.map_err(|e| e.to_string())?;
    let msg = &j["choices"][0]["message"];
    let content = msg["content"].as_str().unwrap_or("").to_string();
    let mut tool_chunks: Vec<(String, String, String)> = Vec::new();
    if let Some(tcs) = msg["tool_calls"].as_array() {
        for tc in tcs {
            let id   = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            let args = tc["function"]["arguments"].as_str().unwrap_or("{}").to_string();
            if !name.is_empty() {
                tool_chunks.push((id, name, args));
            }
        }
    }
    Ok((content, tool_chunks))
}

/// Detect whether a response contains a reusable structured framework.
/// Used by interactive.rs to decide whether to emit `agent:skill_suggestion`.
pub(crate) fn detect_response_framework(text: &str) -> bool {
    let has_numbered = (text.contains("1.") || text.contains("1、") || text.contains("①"))
        && (text.contains("2.") || text.contains("2、") || text.contains("②"));
    let has_sequential = (text.contains("先") && text.contains("再") && text.contains("最後"))
        || (text.contains("首先") && text.contains("接著"));
    let has_framework_kw = text.contains("步驟") || text.contains("流程") || text.contains("規範");
    text.len() > 300 && (has_numbered || has_sequential || has_framework_kw)
}
