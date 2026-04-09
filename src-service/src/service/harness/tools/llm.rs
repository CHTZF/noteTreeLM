use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::service::harness::observability::emitter::ObservabilityEmitter;
use crate::service::harness::memory::working::WorkingMemory;

/// Maximum byte length of the content inside a `[cite:...]` tag.
/// OpenAI tool_call_id ≤ 29 chars; multiple IDs + commas stay well under 128.
/// LLM not complying → treated as plain text after this limit.
const MAX_CITE_INNER_LEN: usize = 128;

/// Number of bytes held back from emitting to detect artifact prefixes.
/// Must be ≥ len("[cite:") + MAX_CITE_INNER_LEN + len("]") = 6 + 128 + 1 = 135.
/// Round up to 160 for safety.
const HOLD_BACK: usize = 160;

/// Remove all `[cite:...]` tags from `text`, collecting the inner content of each.
/// Returns `(cleaned_text, collected_inners)`.
/// Used for `full_text` post-processing and for `llm:done` payload.
fn strip_and_collect_cite_tags(text: &str) -> (String, Vec<String>) {
    let mut result  = String::new();
    let mut inners  = Vec::new();
    let mut rest    = text;
    while let Some(start) = rest.find("[cite:") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 6..]; // skip "[cite:"
        if let Some(end) = after.find(']') {
            let inner = after[..end].trim().to_string();
            if !inner.is_empty() { inners.push(inner); }
            rest = &after[end + 1..];
        } else {
            // Unclosed tag — drop the "[cite:" prefix, keep the rest
            rest = after;
            break;
        }
    }
    result.push_str(rest);
    (result.trim_end().to_string(), inners)
}

/// Convenience wrapper: strip cite tags without collecting inners.
fn strip_all_cite_tags(text: &str) -> String {
    strip_and_collect_cite_tags(text).0
}

/// Find the largest byte offset in `s` that is a valid UTF-8 char boundary
/// and is ≤ `max_bytes`.
fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() { return s.len(); }
    let mut i = max_bytes;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    i
}

/// Scan `buf` for complete `[cite:...]` tags, strip them, collect inner content.
/// Returns `(clean_text, leftover_prefix, collected_inners)`:
/// - `clean_text`:       portion with all complete cite tags removed, safe to emit
/// - `leftover_prefix`:  suffix of `buf` that starts a possible-but-incomplete cite tag
///                       (must be moved back to hold-back, not emitted yet)
/// - `collected_inners`: inner content of each stripped tag
fn scan_and_strip_cites(buf: &str) -> (String, String, Vec<String>) {
    let mut clean   = String::new();
    let mut inners  = Vec::new();
    let mut rest    = buf;

    loop {
        match rest.find("[cite:") {
            None => {
                // No cite tag start — check if the tail is a partial prefix of "[cite:"
                let leftover = trailing_cite_prefix(rest);
                let emit_end = rest.len() - leftover.len();
                clean.push_str(&rest[..emit_end]);
                return (clean, leftover.to_string(), inners);
            }
            Some(start) => {
                clean.push_str(&rest[..start]);
                let after_start = &rest[start + 6..]; // skip "[cite:"
                match after_start.find(']') {
                    Some(end) if end <= MAX_CITE_INNER_LEN => {
                        // Complete tag — strip and collect
                        let inner = after_start[..end].trim().to_string();
                        if !inner.is_empty() { inners.push(inner); }
                        rest = &after_start[end + 1..];
                    }
                    Some(_) => {
                        // ']' found but inner is too long — LLM not complying.
                        // Treat "[cite:" as plain text and continue scanning.
                        clean.push_str("[cite:");
                        rest = after_start;
                    }
                    None => {
                        // No ']' yet — this is either an incomplete tag or a very long one.
                        let remaining = after_start.len();
                        if remaining > MAX_CITE_INNER_LEN {
                            // Too long — give up, treat as plain text
                            clean.push_str("[cite:");
                            rest = after_start;
                        } else {
                            // Could still be completed — hold back from "[cite:" onward
                            let leftover = &rest[start..]; // "[cite:..."
                            return (clean, leftover.to_string(), inners);
                        }
                    }
                }
            }
        }
    }
}

/// Return the longest suffix of `s` that is a proper prefix of `"[cite:"`.
/// e.g. "hello[" → "[",  "hello[c" → "[c",  "hello" → ""
fn trailing_cite_prefix(s: &str) -> &str {
    const TAG: &str = "[cite:";
    let bytes = s.as_bytes();
    let tag   = TAG.as_bytes();
    for suffix_len in (1..=tag.len().min(bytes.len())).rev() {
        let suffix = &bytes[bytes.len() - suffix_len..];
        if tag.starts_with(suffix) {
            return &s[s.len() - suffix_len..];
        }
    }
    ""
}

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
///
/// Checks both the tool_call_id keys (outer `__cite_id__` from `annotate_cite_id`) and
/// any nested `__cite_id__` values inside tool results (e.g. `kb_1`, `kb_2` from
/// `search_kb_pages`). This ensures validation survives context compression: WorkingMemory
/// is never cleared during a session, so IDs from compressed rounds remain valid.
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
    let valid = wm.all_valid_cite_ids().await;
    ids.iter().all(|id| valid.contains(id.as_str()))
}

/// Stream one LLM round, emitting llm:token events.
/// Returns (text, finish_reason, tool_chunks, cite_invalid).
/// `cite_invalid` is true when the LLM produced a fabricated [cite:...] that did not
/// match any working-memory evidence ID, so the caller can inject a correction.
pub(crate) async fn stream_llm_round(
    client: &reqwest::Client,
    llm_url: &str,
    body: Value,
    emitter: &ObservabilityEmitter,
    cancel: &Arc<AtomicBool>,
    working_memory: Option<&WorkingMemory>,
) -> Result<(String, String, Vec<(String, String, String)>, bool), String> {
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

    // pending_emit: tail hold-back buffer.
    // We keep the last HOLD_BACK bytes un-emitted so that:
    //   (a) [cite:...] tags anywhere in the output can be stripped before the user sees them
    //   (b) partial <tool_call> prefixes are not accidentally emitted
    // suppress_emit: true once we detect a text-format <tool_call> tag (Qwen/Mistral style).
    let mut pending_emit = String::new();
    let mut suppress_emit = false;
    // cite tags stripped during streaming, collected for post-stream validation
    let mut collected_cites: Vec<String> = Vec::new();

    // ── Helper: process the safe (non-held-back) portion of pending_emit ─────
    // Strips cite tags, detects <tool_call>, emits clean tokens.
    // Returns true if suppress_emit was set.
    macro_rules! flush_safe {
        () => {{
            if suppress_emit { } else {
                // Compute how many bytes to hold back
                let hold = HOLD_BACK.min(pending_emit.len());
                let safe_end = floor_char_boundary(&pending_emit, pending_emit.len() - hold);
                if safe_end > 0 {
                    let safe_portion = pending_emit[..safe_end].to_string();
                    pending_emit = pending_emit[safe_end..].to_string();

                    // ── Artifact scanner: strip [cite:...] ───────────────────
                    let (clean, leftover, cites) = scan_and_strip_cites(&safe_portion);
                    collected_cites.extend(cites);
                    // leftover is a partial [cite: prefix — move back to hold-back
                    let to_emit = if leftover.is_empty() {
                        clean
                    } else {
                        pending_emit.insert_str(0, &leftover);
                        clean
                    };

                    // ── tool_call suppressor ─────────────────────────────────
                    if to_emit.contains("<tool_call>") {
                        let pos = to_emit.find("<tool_call>").unwrap_or(0);
                        if pos > 0 {
                            emitter.emit("llm:token".to_string(), json!(&to_emit[..pos]));
                        }
                        pending_emit.clear();
                        suppress_emit = true;
                    } else {
                        // Hold back potential <tool_call> prefix at the tail of to_emit
                        let tc_safe = safe_emit_end(&to_emit, "<tool_call>");
                        if tc_safe > 0 {
                            emitter.emit("llm:token".to_string(), json!(&to_emit[..tc_safe]));
                        }
                        if tc_safe < to_emit.len() {
                            pending_emit.insert_str(0, &to_emit[tc_safe..]);
                        }
                    }
                }
            }
        }};
    }

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
                                    pending_emit.push_str(content);
                                    flush_safe!();
                                }
                            }
                        }
                        // Native format tool calls (OpenAI-compatible)
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

    // ── Stream end flush ──────────────────────────────────────────────────────
    // pending_emit still holds HOLD_BACK bytes (or less if response was short).
    // Run the full artifact scanner + emit whatever is clean.
    if !suppress_emit && !pending_emit.is_empty() {
        let (clean, _leftover, cites) = scan_and_strip_cites(&pending_emit);
        collected_cites.extend(cites);
        // _leftover here is an incomplete [cite: at the very end — drop it (LLM truncated)
        if !clean.is_empty() {
            // Still apply tool_call guard even at flush
            let tc_safe = safe_emit_end(&clean, "<tool_call>");
            emitter.emit("llm:token".to_string(), json!(&clean[..tc_safe]));
        }
        pending_emit.clear();
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

    // ── Post-stream validation ────────────────────────────────────────────────
    // Stream is fully consumed. Validate cite IDs collected during streaming.
    // We do this here (not during streaming) so:
    //   (a) validate_citation is async but not in the hot streaming path
    //   (b) correction loop in run_tool_loop still works (we return cite_invalid)
    //   (c) agent:citation is only emitted after validation passes
    let mut cite_invalid = false;
    if !collected_cites.is_empty() {
        for cite_inner in &collected_cites {
            if cite_inner == "none" {
                // [cite:none] is always valid — no tools used this round
                continue;
            }
            let valid = validate_citation(cite_inner, working_memory).await;
            if valid {
                // Emit citation event so frontend can display source chips
                let ids: Vec<&str> = cite_inner.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
                emitter.emit("agent:citation".to_string(), json!({ "ids": ids }));
            } else {
                tracing::warn!("[citation] invalid cite ids: [cite:{}]", cite_inner);
                cite_invalid = true;
                emitter.emit("agent:citation_missing".to_string(), json!({}));
            }
        }
    }

    // Strip cite tags from full_text for storage and llm:done payload.
    let (full_text, _) = strip_and_collect_cite_tags(&full_text);

    Ok((full_text, finish_reason, tool_chunks, cite_invalid))
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
