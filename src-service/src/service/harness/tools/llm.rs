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

/// Normalise cite tag variants the LLM commonly produces into the canonical `[cite:` form.
///
/// Handles:
///   `[cita:`  — LLM drops 'e' (most common)
///   `[ cite:` — LLM adds space after bracket
///   `[ cita:` — both variants combined
///   `[CITE:`  — uppercase (case-insensitive normalisation)
fn normalize_cite_variants(text: &str) -> String {
    // Use a single-pass replace chain; order matters only to avoid double-replacement.
    text
        .replace("[ cita:", "[cite:")
        .replace("[cita:", "[cite:")
        .replace("[ cite:", "[cite:")
        // Case-insensitive: lowercase the prefix if the LLM used uppercase.
        // Only handles the common ALL-CAPS case; mixed case is handled below.
        .replace("[CITE:", "[cite:")
        .replace("[CITA:", "[cite:")
}

/// Remove all `[cite:...]` tags from `text`, collecting the inner content of each.
/// Also strips bare `cite:xxx` (without brackets) that small models sometimes output.
/// Returns `(cleaned_text, collected_inners)`.
/// Used for `full_text` post-processing and for `llm:done` payload.
fn strip_and_collect_cite_tags(text: &str) -> (String, Vec<String>) {
    // Normalise variants before processing so [cita:id] is treated identically to [cite:id].
    let text = normalize_cite_variants(text);
    let text = text.as_str();
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
    // Also strip bare `cite:xxx` patterns (no brackets) that small models output,
    // and collect their inners for validation (so correction loop can fire).
    let (cleaned, bare_inners) = strip_bare_cite_tags(&result);
    inners.extend(bare_inners);
    // Strip bracketed malformed tags like [cite_id1] where model used _ instead of :
    let cleaned = strip_malformed_cite_brackets(&cleaned);
    // Strip [tool_name:tool_call_id] patterns where model used tool name instead of "cite"
    // e.g. [web_search:wEZxy8LBBpi55PQjmqUK4Uspp1XkIixh]
    let cleaned = strip_tool_name_cite_brackets(&cleaned);
    (cleaned.trim_end().to_string(), inners)
}

/// Strip bare `cite:xxx` tokens (without surrounding `[` `]`) from text.
/// Returns `(cleaned_text, collected_inners)` — inners are fed into validation
/// so correction loop can fire even when the model omits brackets.
fn strip_bare_cite_tags(text: &str) -> (String, Vec<String>) {
    // Normalise variants so bare `cita:xxx` is treated as `cite:xxx`.
    let text = normalize_cite_variants(text);
    let text = text.as_str();
    let mut result = String::new();
    let mut inners = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("cite:") {
        let is_bracketed = start > 0 && rest.as_bytes().get(start - 1) == Some(&b'[');
        if is_bracketed {
            let end = start + 5;
            result.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        result.push_str(&rest[..start]);
        let after = &rest[start + 5..]; // skip "cite:"
        let token_end = after.find(|c: char| c.is_whitespace()).unwrap_or(after.len());
        let inner = after[..token_end].trim().to_string();
        if !inner.is_empty() { inners.push(inner); }
        let skip_ws = if after[token_end..].starts_with(' ') || after[token_end..].starts_with('\n') { 1 } else { 0 };
        rest = &after[token_end + skip_ws..];
    }
    result.push_str(rest);
    (result, inners)
}

/// Strip malformed bracketed cite tags like `[cite_id1]` where the model used
/// `_` instead of `:`. Matches `[cite` followed by non-`]` chars up to `]`.
fn strip_malformed_cite_brackets(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("[cite") {
        // Check the char right after "[cite" — if it's `:` it was already handled above.
        let after_tag = &rest[start + 5..];
        if after_tag.starts_with(':') {
            // Already handled by strip_and_collect_cite_tags — keep and advance past it
            if let Some(end) = after_tag.find(']') {
                result.push_str(&rest[..start + 6 + end + 1]);
                rest = &after_tag[end + 1..];
            } else {
                result.push_str(&rest[..start + 5]);
                rest = after_tag;
            }
            continue;
        }
        // Malformed: strip up to and including the closing `]`
        result.push_str(&rest[..start]);
        if let Some(end) = after_tag.find(']') {
            rest = &after_tag[end + 1..];
        } else {
            // No closing bracket — drop "[cite" prefix, keep the rest
            rest = after_tag;
        }
    }
    result.push_str(rest);
    result
}

/// Strip malformed citation brackets where the model references tool call IDs directly.
/// Handles two patterns:
///   1. `[tool_name:tool_call_id]` — snake_case name + colon + 16+ alphanumeric ID
///      e.g. `[web_search:wEZxy8LBBpi55PQjmqUK4Uspp1XkIixh]`
///   2. `[tool_call_id]` — bare 16+ alphanumeric ID with no prefix
///      e.g. `[9TcC6KKSkwlhSC5ynDU0MCuXLevxdBlr]`
fn strip_tool_name_cite_brackets(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let after_bracket = &rest[start + 1..];

        // Case 1: [snake_name:long_id]
        let name_end = after_bracket
            .find(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_')
            .unwrap_or(after_bracket.len());
        let name = &after_bracket[..name_end];
        if name.len() >= 2 && after_bracket[name_end..].starts_with(':') {
            let after_colon = &after_bracket[name_end + 1..];
            let id_end = after_colon
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(after_colon.len());
            if id_end >= 16 && after_colon[id_end..].starts_with(']') {
                result.push_str(&rest[..start]);
                rest = &after_colon[id_end + 1..];
                continue;
            }
        }

        // Case 2: [long_alphanumeric_id] — bare tool_call_id with no prefix
        let id_end = after_bracket
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(after_bracket.len());
        if id_end >= 16 && after_bracket[id_end..].starts_with(']') {
            result.push_str(&rest[..start]);
            rest = &after_bracket[id_end + 1..];
            continue;
        }

        result.push_str(&rest[..start + 1]);
        rest = after_bracket;
    }
    result.push_str(rest);
    result
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
    // Normalise cite variants BEFORE scanning so [cita:id] is treated as [cite:id].
    // We operate on the owned normalised string; `rest` slices into it.
    let normalised = normalize_cite_variants(buf);
    let buf = normalised.as_str();
    let mut clean   = String::new();
    let mut inners  = Vec::new();
    let mut rest    = buf;

    loop {
        match rest.find("[cite:") {
            None => {
                // No cite tag start — check if the tail is a partial prefix of "[cite:"
                let leftover = trailing_cite_prefix(rest);
                let emit_end = rest.len() - leftover.len();
                let (bare_clean, bare_inners) = strip_bare_cite_tags(&rest[..emit_end]);
                clean.push_str(&bare_clean);
                inners.extend(bare_inners);
                return (clean, leftover.to_string(), inners);
            }
            Some(start) => {
                let (bare_clean, bare_inners) = strip_bare_cite_tags(&rest[..start]);
                clean.push_str(&bare_clean);
                inners.extend(bare_inners);
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

// ── Citation similarity helpers ───────────────────────────────────────────────

/// Extract content tokens from a JSON value tree (recursively over string/number leaves).
/// - Latin/alphanumeric sequences ≥ 3 chars → lowercased word token
/// - CJK / Hangul / Hiragana / Katakana → each character is a token
fn extract_json_value_tokens(v: &Value, out: &mut std::collections::HashSet<String>) {
    match v {
        Value::String(s) => extract_text_tokens_into(s, out),
        Value::Number(n) => { out.insert(n.to_string()); }
        Value::Array(arr) => { for item in arr { extract_json_value_tokens(item, out); } }
        Value::Object(map) => { for val in map.values() { extract_json_value_tokens(val, out); } }
        _ => {}
    }
}

fn extract_text_tokens_into(text: &str, out: &mut std::collections::HashSet<String>) {
    let mut word = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            word.push(c);
        } else {
            if word.chars().count() >= 3 {
                out.insert(word.to_lowercase());
            }
            word.clear();
            let cp = c as u32;
            // CJK unified ideographs, Hiragana, Katakana, Hangul syllables
            if (0x4E00..=0x9FFF).contains(&cp)
                || (0x3040..=0x30FF).contains(&cp)
                || (0xAC00..=0xD7AF).contains(&cp)
            {
                out.insert(c.to_string());
            }
        }
    }
    if word.chars().count() >= 3 {
        out.insert(word.to_lowercase());
    }
}

/// Return `false` when the LLM response has suspiciously low token overlap with the
/// combined content of all cited tool results — a signal that the response was fabricated.
///
/// Threshold: at least 15% of unique response tokens must appear in the cited results.
/// Short responses (< 80 chars) or results with < 15 unique tokens are skipped (not enough
/// signal to distinguish legitimate paraphrase from fabrication).
async fn check_response_cite_similarity(
    response: &str,
    cite_inner: &str,
    working_memory: Option<&WorkingMemory>,
) -> bool {
    let wm = match working_memory {
        Some(w) => w,
        None    => return true,
    };
    if cite_inner.trim() == "none" { return true; }
    // Skip very short responses — not enough signal.
    if response.chars().count() < 80 { return true; }

    let ids: Vec<&str> = cite_inner.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    // Collect result values for all cited IDs (top-level tool_call_id keys only).
    let results: Vec<Value> = wm.with_records(|map| {
        ids.iter()
            .filter_map(|id| map.get(*id).map(|r| r.result.clone()))
            .collect()
    }).await;

    if results.is_empty() { return true; } // nested __cite_id__ — skip check

    let mut result_tokens: std::collections::HashSet<String> = std::collections::HashSet::new();
    for rv in &results {
        extract_json_value_tokens(rv, &mut result_tokens);
    }
    // Not enough data in the result to make a meaningful comparison.
    if result_tokens.len() < 15 { return true; }

    let mut response_tokens: std::collections::HashSet<String> = std::collections::HashSet::new();
    extract_text_tokens_into(response, &mut response_tokens);

    if response_tokens.is_empty() { return true; }

    let overlap = response_tokens.iter().filter(|t| result_tokens.contains(*t)).count();
    let score   = overlap as f32 / response_tokens.len() as f32;

    tracing::debug!(
        "[citation/similarity] cite={} score={:.3} overlap={}/{} result_tokens={}",
        cite_inner, score, overlap, response_tokens.len(), result_tokens.len()
    );

    score >= 0.15
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
    tool_names: &[String],
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

    // ── Streaming state ───────────────────────────────────────────────────────
    // pending_emit: hold-back buffer for the last HOLD_BACK bytes so that
    //   [cite:...] tags and partial <tool_call> prefixes are not prematurely emitted.
    // json_buf / json_depth: JSON object accumulator.
    //   Once a '{' is seen in delta.content we stop emitting and buffer the entire
    //   JSON object. When depth returns to 0, we inspect the parsed result:
    //     - name matches a known tool → treat as tool call (fill tool_chunks)
    //     - otherwise → flush as regular text
    //   This handles both "proper" <tool_call>…</tool_call> format AND the case
    //   where <tool_call> is a special GGUF token decoded to garbage ("leton"):
    //   the JSON object is still intact and follows immediately.
    // suppress_emit: set when we enter JSON-buffering mode or detect <tool_call>.
    //   Cleared when JSON object is resolved (either as tool or as text).
    let tool_name_set: std::collections::HashSet<&str> =
        tool_names.iter().map(String::as_str).collect();
    let mut pending_emit = String::new();
    let mut suppress_emit = false;
    let mut json_buf = String::new();
    let mut json_depth: i32 = 0;
    // cite tags stripped during streaming, collected for post-stream validation
    let mut collected_cites: Vec<String> = Vec::new();
    // text accumulated before the JSON object started (preamble to discard)
    let mut pre_json_buf = String::new();
    let mut tc_text_idx = 0usize;

    // Emit text through the cite-stripping pipeline and HOLD_BACK buffer.
    macro_rules! emit_text {
        ($text:expr) => {{
            let s: &str = $text;
            if !s.is_empty() {
                let hold = HOLD_BACK.min(pending_emit.len() + s.len());
                pending_emit.push_str(s);
                let safe_end = floor_char_boundary(&pending_emit, pending_emit.len().saturating_sub(hold));
                if safe_end > 0 {
                    let safe_portion = pending_emit[..safe_end].to_string();
                    pending_emit = pending_emit[safe_end..].to_string();
                    let (clean, leftover, cites) = scan_and_strip_cites(&safe_portion);
                    collected_cites.extend(cites);
                    if !leftover.is_empty() {
                        pending_emit.insert_str(0, &leftover);
                    }
                    if !clean.is_empty() {
                        emitter.emit("llm:token".to_string(), json!(clean));
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
                                // ── JSON depth tracker ───────────────────────
                                // Walk the incoming chunk character by character.
                                // Once we see '{' we enter JSON-buffering mode and
                                // stop emitting. Strings inside JSON are handled by
                                // tracking whether we're inside a quoted string.
                                let mut chunk_rest = content;
                                while !chunk_rest.is_empty() {
                                    if json_depth == 0 {
                                        // Not yet inside a JSON object.
                                        if let Some(brace) = chunk_rest.find('{') {
                                            // Only enter JSON-buffering mode if the character
                                            // immediately after '{' (ignoring whitespace) is '"'.
                                            // Tool call JSON always starts with {"name": ...
                                            // This avoids buffering informal text like {xxxx}
                                            // or {中文} which would cause streaming delays or
                                            // silent text loss if the brace is unmatched.
                                            let after_brace = chunk_rest[brace + 1..].trim_start();
                                            let looks_like_json = after_brace.starts_with('"')
                                                || after_brace.is_empty(); // brace at end of chunk — wait for next
                                            if !looks_like_json {
                                                // Not a JSON object — emit '{' as regular text and continue.
                                                let up_to_and_including_brace = &chunk_rest[..brace + 1];
                                                if !suppress_emit {
                                                    emit_text!(up_to_and_including_brace);
                                                }
                                                chunk_rest = &chunk_rest[brace + 1..];
                                                continue;
                                            }
                                            // Emit everything before '{' as normal text.
                                            let before = &chunk_rest[..brace];
                                            if !suppress_emit && !before.is_empty() {
                                                emit_text!(before);
                                            }
                                            // Start buffering JSON.
                                            json_depth = 1;
                                            suppress_emit = true;
                                            pre_json_buf.clear();
                                            json_buf.clear();
                                            json_buf.push('{');
                                            chunk_rest = &chunk_rest[brace + 1..];
                                        } else {
                                            // No '{' in this chunk — emit normally.
                                            if !suppress_emit {
                                                emit_text!(chunk_rest);
                                            }
                                            chunk_rest = "";
                                        }
                                    } else {
                                        // Inside a JSON object — buffer everything, track depth.
                                        // We need to respect string boundaries so that '{' / '}'
                                        // inside string values don't affect depth.
                                        let mut i = 0;
                                        let bytes = chunk_rest.as_bytes();
                                        let mut in_string = false;
                                        let mut escape = false;
                                        while i < bytes.len() {
                                            let b = bytes[i];
                                            if escape {
                                                escape = false;
                                            } else if b == b'\\' && in_string {
                                                escape = true;
                                            } else if b == b'"' {
                                                in_string = !in_string;
                                            } else if !in_string {
                                                if b == b'{' { json_depth += 1; }
                                                else if b == b'}' {
                                                    json_depth -= 1;
                                                    if json_depth == 0 {
                                                        // Closing brace found — complete object.
                                                        json_buf.push('}');
                                                        // Consume up to and including '}'
                                                        chunk_rest = &chunk_rest[i + 1..];

                                                        // ── Resolve: tool call or plain text? ──
                                                        let resolved = if let Ok(parsed) = serde_json::from_str::<Value>(&json_buf) {
                                                            let name = parsed["name"].as_str().unwrap_or("").to_string();
                                                            if !name.is_empty() && tool_name_set.contains(name.as_str()) {
                                                                // Recognised tool call.
                                                                let args = &parsed["arguments"];
                                                                let args_str = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
                                                                tracing::debug!("[llm/stream] json-tracker tool call: name={}", name);
                                                                tool_chunks.push((format!("tc_text_{}", tc_text_idx), name, args_str));
                                                                tc_text_idx += 1;
                                                                // Discard any remaining </tool_call> on this chunk
                                                                let after = chunk_rest.trim_start();
                                                                if after.starts_with("</tool_call>") {
                                                                    chunk_rest = &chunk_rest[chunk_rest.find("</tool_call>").unwrap() + "</tool_call>".len()..];
                                                                }
                                                                true // suppress — do not emit json_buf
                                                            } else {
                                                                false // not a tool call
                                                            }
                                                        } else {
                                                            false // parse failed
                                                        };

                                                        if !resolved {
                                                            // Not a tool call — emit the buffered JSON as text.
                                                            let text_to_emit = json_buf.clone();
                                                            suppress_emit = false;
                                                            emit_text!(&text_to_emit);
                                                        } else {
                                                            suppress_emit = false;
                                                        }
                                                        json_buf.clear();
                                                        break; // restart outer while with updated chunk_rest
                                                    }
                                                }
                                            }
                                            json_buf.push(b as char);
                                            i += 1;
                                        }
                                        if json_depth > 0 {
                                            // Object not yet complete — consumed entire chunk.
                                            chunk_rest = "";
                                        }
                                    }
                                }
                            }
                        }
                        // Native format tool calls (OpenAI-compatible delta.tool_calls).
                        // When llama.cpp correctly parses the hermes/qwen tool call format,
                        // the structured call appears here — not in delta.content.
                        if let Some(tc_arr) = delta["tool_calls"].as_array() {
                            for tc in tc_arr {
                                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                                while tool_chunks.len() <= idx {
                                    tool_chunks.push((String::new(), String::new(), String::new()));
                                }
                                let acc = &mut tool_chunks[idx];
                                if let Some(id) = tc["id"].as_str() {
                                    if !id.is_empty() {
                                        if acc.0.is_empty() {
                                            tracing::debug!("[llm/stream] native tool_call[{}] id={}", idx, id);
                                        }
                                        acc.0 = id.to_string();
                                    }
                                }
                                if let Some(n) = tc["function"]["name"].as_str() {
                                    if !n.is_empty() {
                                        if acc.1.is_empty() {
                                            tracing::debug!("[llm/stream] native tool_call[{}] name={}", idx, n);
                                        }
                                        acc.1 = n.to_string();
                                    }
                                }
                                if let Some(a) = tc["function"]["arguments"].as_str() { acc.2.push_str(a); }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Stream end flush ──────────────────────────────────────────────────────
    // Emit whatever remains in pending_emit (cite-strip it first).
    // If json_depth > 0, the stream was cut mid-JSON — try to parse what we have.
    if json_depth > 0 && !json_buf.is_empty() {
        // Stream ended while we were buffering a JSON object.
        // Try to parse what we have as a tool call (e.g. max_tokens truncation mid-JSON).
        let resolved = if let Ok(parsed) = serde_json::from_str::<Value>(&json_buf) {
            let name = parsed["name"].as_str().unwrap_or("").to_string();
            if !name.is_empty() && tool_name_set.contains(name.as_str()) {
                let args = &parsed["arguments"];
                let args_str = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
                tracing::debug!("[llm/stream] json-tracker truncated tool call: name={}", name);
                tool_chunks.push((format!("tc_text_{}", tc_text_idx), name, args_str));
                true
            } else { false }
        } else { false };
        if !resolved {
            // Not a tool call (or parse failed) — the buffered content is regular text.
            // Emit it so the user sees it rather than silently losing it.
            let leftover = json_buf.clone();
            emit_text!(&leftover);
        }
        json_buf.clear();
        suppress_emit = false;
    }
    if !suppress_emit && !pending_emit.is_empty() {
        let (clean, _leftover, cites) = scan_and_strip_cites(&pending_emit);
        collected_cites.extend(cites);
        if !clean.is_empty() {
            emitter.emit("llm:token".to_string(), json!(clean));
        }
        pending_emit.clear();
    }

    // Tool call parsing is now done during streaming via the JSON depth tracker.
    // full_text at this point contains only non-tool-call text (preamble was discarded
    // when the JSON was resolved as a tool call in the streaming loop above).

    // ── Tool-call round cleanup ───────────────────────────────────────────────
    // Unified preamble elimination: whenever tools ran this round (either native
    // delta.tool_calls format or text <tool_call> format), the model may have
    // emitted noise text ("leton", "Let me check…", etc.) before the tool call.
    // That text is never a valid final answer and must not reach the frontend or
    // the stored response. Handle it in ONE place here rather than patching each
    // individual flush path:
    //   1. Discard full_text — prevents preamble from becoming full_response.
    //   2. Emit agent:clear_stream — clears whatever already streamed to frontend
    //      from the mid-stream or stream-end flush paths this round.
    // The stream-end flush and mid-stream flush_safe! still suppress most
    // preamble proactively, but this is the authoritative safety net.
    if !tool_chunks.is_empty() {
        full_text = String::new();
        emitter.emit("agent:clear_stream".to_string(), json!({}));
    }

    // ── Post-stream validation ────────────────────────────────────────────────
    // Stream is fully consumed. Validate cite IDs collected during streaming.
    // Rules (enforced when working_memory is Some, i.e. tool-enabled agents):
    //   1. Every non-empty final answer MUST include [cite:none] or [cite:valid_id].
    //   2. If no cite tag present at all → cite_invalid = true (forces correction loop).
    //   3. If cite tag present with unknown ID → cite_invalid = true.
    // This prevents LLM from fabricating tool results without going through correction.
    let mut cite_invalid = false;
    // If the full_text contains <tool_call> markers, the LLM tried to call a tool
    // but it was truncated or malformed (tool_chunks is empty). This is not a valid
    // final answer — treat it as no-output so cite validation and cite_status are skipped.
    // Also check for </tool_call> alone (opening tag missing): model sometimes skips
    // the opening tag, leaving raw JSON + closing tag in full_text.
    // Tool call syntax is resolved during streaming by the JSON depth tracker.
    // full_text no longer contains raw <tool_call>/<tool_call> fragments — they were
    // consumed during streaming. The only remaining case where full_text might contain
    // stray </tool_call> is if the native delta.tool_calls path ran and some closing
    // tag leaked into content. Discard such remnants.
    if full_text.contains("</tool_call>") || full_text.contains("<tool_call>") {
        full_text = String::new();
        emitter.emit("agent:clear_stream".to_string(), json!({}));
    }
    let is_final_answer = tool_chunks.is_empty() && !full_text.trim().is_empty();
    // Enforce cite when either:
    // (a) tools ran this round (run_has_results), OR
    // (b) WM has accumulated data from previous rounds in this session (wm_has_any) —
    //     the LLM may be answering from carry-over results without calling tools,
    //     which is still fabrication if no cite tag is present.
    // Exception: if wm_has_any but not run_has_results, the LLM *could* legitimately
    // answer a pure follow-up question (e.g. "translate that") — so we only block if
    // the response looks data-bearing (> 120 chars). Short acknowledgements are allowed.
    let (run_has_results, wm_has_any) = match working_memory {
        Some(wm) => (wm.current_run_has_results().await, !wm.is_empty().await),
        None     => (false, false),
    };
    let cite_required = if run_has_results {
        true
    } else if wm_has_any && full_text.chars().count() > 120 {
        true
    } else {
        false
    };
    if is_final_answer && cite_required && collected_cites.is_empty() {
        // LLM produced a substantive answer with NO cite tag at all — treat as fabrication.
        tracing::warn!("[citation] missing cite tag in final answer — triggering correction");
        cite_invalid = true;
        emitter.emit("agent:citation_missing".to_string(), json!({}));
    } else if !collected_cites.is_empty() {
        // Deduplicate: LLM sometimes repeats the same [cite:id] multiple times.
        let mut seen = std::collections::HashSet::new();
        let collected_cites: Vec<_> = collected_cites.into_iter().filter(|id| seen.insert(id.clone())).collect();

        // Two-pass validation:
        // Pass 1: check all IDs and track whether at least one valid+similar cite exists.
        // Pass 2: only trigger correction if NO valid cite passed — invalid IDs mixed in
        //         alongside a valid one are a format mistake (e.g. LLM confused Gmail
        //         message IDs with WM tool_call_ids), not fabrication.
        let has_none = collected_cites.iter().any(|id| id == "none");
        let mut any_valid_and_similar = has_none; // [cite:none] counts as valid
        let mut valid_details: Vec<Value> = Vec::new();
        let mut valid_ids: Vec<String> = Vec::new();

        for cite_inner in &collected_cites {
            if cite_inner == "none" { continue; }
            let valid = validate_citation(cite_inner, working_memory).await;
            if valid {
                let similar = check_response_cite_similarity(&full_text, cite_inner, working_memory).await;
                if similar {
                    any_valid_and_similar = true;
                    valid_ids.push(cite_inner.clone());
                    // Collect details for frontend expand UI
                    let ids: Vec<&str> = cite_inner.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
                    if let Some(wm) = working_memory {
                        for id in &ids {
                            if let Some((tool, preview)) = wm.cite_detail(id).await {
                                valid_details.push(json!({ "id": id, "tool": tool, "preview": preview }));
                            }
                        }
                    }
                } else {
                    tracing::warn!("[citation] response has low content overlap with cited result [cite:{}]", cite_inner);
                }
            } else {
                tracing::warn!("[citation] invalid cite ids: [cite:{}]", cite_inner);
            }
        }

        if any_valid_and_similar {
            // At least one cite ID is valid and content-consistent — accept the response.
            let ids_ref: Vec<&str> = valid_ids.iter().map(String::as_str).collect();
            emitter.emit("agent:citation".to_string(), json!({ "ids": ids_ref, "details": valid_details }));
        } else {
            // No valid cite passed — trigger correction.
            tracing::warn!("[citation] no valid cite found — triggering correction");
            cite_invalid = true;
            emitter.emit("agent:citation_missing".to_string(), json!({}));
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
        "max_tokens": 4096,
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
