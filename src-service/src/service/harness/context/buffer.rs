use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use serde_json::{json, Value};
use super::super::prompt::{templates, Locale};

/// Manages the live message buffer and finish-signal for a single agent tool loop.
///
/// Deliberately has no dependency on `WorkingMemory` — methods that need stall detection
/// or compress summaries live as thin orchestrators on `HarnessRequestRuntime`, which
/// has access to both this buffer and `working_memory`.
#[derive(Clone)]
pub(crate) struct ContextBuffer {
    msgs_buf:      Arc<Mutex<Vec<Value>>>,
    finish_answer: Arc<Mutex<Option<String>>>,
}

impl ContextBuffer {
    /// Byte threshold at which a context-usage warning is injected each round.
    pub(crate) const CONTEXT_WARN_BYTES:     usize = 28_000;
    /// Byte threshold above which the context is considered critical; auto-compress triggers.
    pub(crate) const CONTEXT_CRITICAL_BYTES: usize = 40_000;
    /// Per-message hard cap for tool results (UTF-8 bytes).
    const TOOL_RESULT_MAX_BYTES: usize = 10_000;

    pub(crate) fn new() -> Self {
        Self {
            msgs_buf:      Arc::new(Mutex::new(Vec::new())),
            finish_answer: Arc::new(Mutex::new(None)),
        }
    }

    // ── Message buffer ────────────────────────────────────────────────────────

    /// Replace the buffer with an assembled context (called once after context pipeline).
    pub(crate) async fn init(&self, messages: Vec<Value>) {
        *self.msgs_buf.lock().await = messages;
    }

    /// Append one message.
    pub(crate) async fn push(&self, msg: Value) {
        self.msgs_buf.lock().await.push(msg);
    }

    /// Clone the current buffer (cheap via Arc).
    pub(crate) async fn snapshot(&self) -> Vec<Value> {
        self.msgs_buf.lock().await.clone()
    }

    /// Approximate serialized byte size of a single message.
    /// Serialises the whole JSON value so `tool_calls`, arguments, etc. are included.
    pub(crate) fn msg_size(m: &Value) -> usize {
        serde_json::to_string(m).map(|s| s.len()).unwrap_or(0)
    }

    /// Total estimated byte size of all buffered messages.
    pub(crate) async fn byte_count(&self) -> usize {
        self.msgs_buf.lock().await.iter().map(Self::msg_size).sum()
    }

    /// Extend buffer with cap-applied messages.
    /// Does NOT check the total budget — callers that need budget enforcement use
    /// `HarnessRequestRuntime::extend_msgs_guarded` instead.
    pub(crate) async fn extend(&self, msgs: Vec<Value>, locale: Locale) {
        let capped: Vec<Value> = msgs.into_iter().map(|m| Self::cap_tool_result(m, locale)).collect();
        self.msgs_buf.lock().await.extend(capped);
    }

    /// Compress the buffer: keep all leading system messages + `tail` recent messages,
    /// replacing everything in between with a single summary system message.
    ///
    /// `keep_ids` — tool_call ids whose assistant+tool pairs must survive the compression
    /// even if they fall outside the tail window.
    pub(crate) async fn compress(&self, summary: &str, tail: usize, keep_ids: &[String], locale: Locale) -> usize {
        let mut msgs = self.msgs_buf.lock().await;
        let n          = msgs.iter().take_while(|m| m["role"] == "system").count();
        let mut tail_start = msgs.len().saturating_sub(tail);
        // Snap backward to a clean turn boundary so the tail never starts with an
        // orphaned "tool" message whose paired assistant{tool_calls} was discarded.
        while tail_start > n + 1 && msgs[tail_start]["role"].as_str() != Some("user") {
            tail_start -= 1;
        }

        // Collect indices of pinned assistant+tool pairs.
        let mut pinned: BTreeSet<usize> = BTreeSet::new();
        if !keep_ids.is_empty() {
            let mut i = n;
            while i < tail_start {
                if msgs[i]["role"] == "assistant" {
                    let ids_match = msgs[i]["tool_calls"].as_array()
                        .map(|tcs| tcs.iter().any(|tc| {
                            keep_ids.iter().any(|kid| tc["id"].as_str() == Some(kid.as_str()))
                        }))
                        .unwrap_or(false);
                    if ids_match {
                        pinned.insert(i);
                        let mut j = i + 1;
                        while j < tail_start && msgs[j]["role"] == "tool" {
                            pinned.insert(j);
                            j += 1;
                        }
                    }
                }
                i += 1;
            }
        }

        let sys:    Vec<Value> = msgs.iter().take(n).cloned().collect();
        let kept:   Vec<Value> = pinned.iter().map(|&i| msgs[i].clone()).collect();
        let tail_v: Vec<Value> = msgs.iter().skip(tail_start).cloned().collect();
        msgs.clear();
        msgs.extend(sys);
        msgs.push(json!({
            "role": "system",
            "content": templates::compress_summary_msg(summary, locale),
        }));
        msgs.extend(kept);
        msgs.extend(tail_v);
        msgs.len()
    }

    // ── finish_answer ────────────────────────────────────────────────────────

    /// Signal the tool loop to stop with `answer` as the final response.
    pub(crate) async fn set_finish(&self, answer: String) {
        *self.finish_answer.lock().await = Some(answer);
    }

    /// Consume the finish signal (returns `Some` once, then `None`).
    pub(crate) async fn take_finish(&self) -> Option<String> {
        self.finish_answer.lock().await.take()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Truncate a single tool result message if its content exceeds `TOOL_RESULT_MAX_BYTES`.
    /// Truncation preserves a line boundary and appends clear markers with total size.
    fn cap_tool_result(msg: Value, locale: Locale) -> Value {
        if msg["role"].as_str() != Some("tool") { return msg; }
        let content = match msg["content"].as_str() {
            Some(c) if c.len() > Self::TOOL_RESULT_MAX_BYTES => c.to_string(),
            _ => return msg,
        };
        let total = content.len();
        let mut safe_end = Self::TOOL_RESULT_MAX_BYTES.min(content.len());
        while !content.is_char_boundary(safe_end) { safe_end -= 1; }
        let cut = content[..safe_end].rfind('\n').unwrap_or(safe_end);
        let truncated = templates::tool_result_truncated(cut, total, &content[..cut], locale);
        let mut out = msg.clone();
        if let Value::Object(ref mut m) = out {
            m.insert("content".to_string(), json!(truncated));
        }
        out
    }
}
