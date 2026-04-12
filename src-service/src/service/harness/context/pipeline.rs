//! Context pipeline: assembles the message list sent to the LLM each turn.
//!
//! ## Slot model
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │  SYSTEM slot  (system prompt + anti-hallucination +      │
//! │                activity_context + skill_injection +       │
//! │                memory facts)                             │
//! ├─────────────────────────────────────────────────────────┤
//! │  HISTORY slot (conversation turns — trimmed if over      │
//! │                budget by summarising oldest turns)       │
//! ├─────────────────────────────────────────────────────────┤
//! │  CURRENT slot (current user input — always kept)        │
//! └─────────────────────────────────────────────────────────┘
//! ```

use serde_json::{json, Value};
use crate::db::SurrealDb;
use super::super::memory::episodic::EpisodicMemory;
use super::history::trim_history;
use super::injectors::{load_exec_history, load_checkpoint, load_agent_knowledge};
use super::super::prompt::{templates, Locale};

// ── Budget ────────────────────────────────────────────────────────────────────

/// Character-based budget for each context slot.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ContextBudget {
    /// Budget chars available for skill injection (step 1e).
    pub system_chars:  usize,
    /// Max chars for the memory block injected into the system message.
    pub memory_chars:  usize,
    /// Max chars for conversation history (triggers summarisation when exceeded).
    pub history_chars: usize,
    /// Number of most-recent turns to always keep verbatim (not summarised).
    pub keep_recent:   usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self::from_context_size(8_192)
    }
}

impl ContextBudget {
    /// Compute a proportional budget from a model's context window (in tokens).
    ///
    /// Allocation (of usable 80% of the window):
    ///   history  65 %  — conversation turns
    ///   system   25 %  — system prompt + skill injection
    ///   memory   10 %  — prefetched memory facts
    ///
    /// Char estimate: 1 token ≈ 3.5 chars (conservative average for mixed CJK/EN text).
    pub fn from_context_size(ctx_tokens: usize) -> Self {
        let usable = (ctx_tokens as f64 * 0.80) as usize;
        let total_chars = (usable as f64 * 3.5) as usize;
        Self {
            history_chars: (total_chars as f64 * 0.65) as usize,
            system_chars:  (total_chars as f64 * 0.25) as usize,
            memory_chars:  (total_chars as f64 * 0.10) as usize,
            keep_recent:   6,
        }
    }
}

// ── Input ─────────────────────────────────────────────────────────────────────

/// All inputs needed to assemble a context window for one LLM turn.
pub(crate) struct ContextInput<'a> {
    pub db:               &'a SurrealDb,
    pub conv_id:          &'a str,
    pub vault_id:         &'a str,
    pub user_input:       &'a str,
    /// Base system prompt from agent_def.
    pub system_prompt:    &'a str,
    /// Optional user activity context (recent UI events).
    pub activity_context: Option<&'a str>,
    /// Memory facts pre-fetched by the parallel pre-pass.
    pub memory_facts:     &'a [Value],
    /// True for interactive chat agents (kind = "chat").
    pub is_chat:          bool,
    /// UI language for localised system messages.
    pub locale:           Locale,
    /// True when the previous message had active skills (tool chain in progress).
    /// When true, tool call/result pairs are included in history for cross-message chaining.
    /// When false, only user and assistant text messages are loaded to keep context lean.
    pub has_cached_skills: bool,
}

// ── Output ────────────────────────────────────────────────────────────────────

/// Result of running the context pipeline: assembled message list + diagnostics.
#[allow(dead_code)]
pub(crate) struct BuiltContext {
    pub messages:                  Vec<Value>,
    pub system_chars_used:         usize,
    pub history_chars_before_trim: usize,
    pub was_trimmed:               bool,
}

// ── Pipeline ──────────────────────────────────────────────────────────────────

pub(crate) struct ContextPipeline {
    pub budget: ContextBudget,
}

impl ContextPipeline {
    pub fn new(budget: ContextBudget) -> Self {
        Self { budget }
    }

    /// Assemble the full message list:
    /// 1. System message (base prompt + activity + memory + skill injection)
    /// 2. Conversation history (loaded from episodic memory, trimmed if over budget)
    /// 3. Injected system messages: exec history, checkpoint, agent knowledge
    pub async fn build(
        &self,
        input:   ContextInput<'_>,
        client:  &reqwest::Client,
        llm_url: &str,
    ) -> BuiltContext {
        // ── Stage 1: System message ────────────────────────────────────────
        let system_content = self.build_system_content(&input);
        let system_chars_used = system_content.len();

        // ── Stage 2: History ───────────────────────────────────────────────
        // When no skill chain is in progress, strip tool call/result pairs from history
        // to keep context lean. When has_cached_skills=true the LLM needs prior tool
        // outputs (e.g. list_emails IDs) to continue the chain — load them with
        // JSON-aware compression so critical fields (IDs, subjects) are always preserved.
        const TOOL_RESULT_LOAD_MAX: usize = 4000;
        let raw_history = EpisodicMemory::new(input.db.clone()).load(input.conv_id).await;
        let history_base: Vec<serde_json::Value> = if input.has_cached_skills {
            raw_history.into_iter().filter_map(|m| {
                match m["role"].as_str() {
                    Some("tool") => {
                        let content = m["content"].as_str().unwrap_or("");
                        if content.len() > TOOL_RESULT_LOAD_MAX {
                            let compressed = compress_tool_result(content, TOOL_RESULT_LOAD_MAX);
                            let mut msg = m;
                            msg["content"] = json!(compressed);
                            Some(msg)
                        } else {
                            Some(m)
                        }
                    }
                    Some("user") | Some("assistant") => Some(m),
                    _ => None,
                }
            }).collect()
        } else {
            // No active skill chain — load only plain user/assistant turns.
            raw_history.into_iter().filter(|m| {
                matches!(m["role"].as_str(), Some("user") | Some("assistant"))
                && m["tool_calls"].is_null()
            }).collect()
        };
        let mut history = history_base;
        if history.last().and_then(|m| m["role"].as_str()) != Some("user") {
            history.push(json!({"role": "user", "content": input.user_input}));
        }
        let history_chars_before_trim: usize = history.iter()
            .map(|m| m["content"].as_str().unwrap_or("").len())
            .sum();

        // ── Stage 3: Trim history if over budget ───────────────────────────
        let (history, was_trimmed) = trim_history(
            history,
            self.budget.history_chars,
            self.budget.keep_recent,
            client,
            llm_url,
            input.locale,
        ).await;

        // ── Stage 4: Supplemental context ────────────────────────────────
        // Merge into the single system message rather than adding extra messages.
        // This satisfies models (e.g. Qwen3.5) that only allow system at position 0,
        // and prevents these strings from being saved to DB as fake user messages.
        let locale = input.locale;
        let (exec_history, checkpoint, agent_knowledge) = tokio::join!(
            load_exec_history(input.db, input.conv_id),
            load_checkpoint(input.db, input.conv_id, locale),
            load_agent_knowledge(input.db, input.vault_id, locale),
        );
        let system_content = {
            let mut parts = vec![system_content];
            if let Some(c) = exec_history    { parts.push(c); }
            if let Some(c) = checkpoint      { parts.push(c); }
            if let Some(c) = agent_knowledge { parts.push(c); }
            parts.join("\n\n")
        };

        // Assemble: single system message → history
        let mut messages = Vec::with_capacity(1 + history.len());
        messages.push(json!({"role": "system", "content": system_content}));
        messages.extend(history);

        BuiltContext { messages, system_chars_used, history_chars_before_trim, was_trimmed }
    }

    // ── System message builder ────────────────────────────────────────────────

    fn build_system_content(&self, input: &ContextInput<'_>) -> String {
        let mut parts: Vec<String> = Vec::new();

        parts.push(input.system_prompt.to_string());

        if let Some(ac) = input.activity_context {
            if !ac.is_empty() {
                parts.push(templates::activity_context_block(ac, input.locale));
            }
        }

        if input.is_chat {
            parts.push(templates::citation_protocol(input.locale).to_string());
        }

        if !input.memory_facts.is_empty() {
            parts.push(self.build_memory_block(input.memory_facts, input.locale));
        }


        parts.join("\n\n")
    }

    fn build_memory_block(&self, facts: &[Value], locale: Locale) -> String {
        let lines: String = facts.iter()
            .map(|f| format!("[{}] {}",
                f["category"].as_str().unwrap_or("general"),
                f["content"].as_str().unwrap_or("")
            ))
            .collect::<Vec<_>>()
            .join("\n");
        templates::memory_block(&lines, locale).chars().take(self.budget.memory_chars).collect()
    }
}

// ── Tool result compression ───────────────────────────────────────────────────

/// Compress a tool result string to fit within `budget` bytes using a two-stage strategy:
///
/// **Stage 1 — JSON-aware field-value compression**
/// Parse as JSON and recursively truncate long string values (> `FIELD_VALUE_MAX` chars).
/// This preserves the complete JSON structure and all short values (IDs, dates, subjects),
/// while compressing only the verbose fields (email bodies, HTML snippets, note content).
/// If the result after field compression is still over budget, and it is a JSON array,
/// trim trailing items and append `{"__more__": N}` so the LLM knows data was cut.
///
/// **Stage 2 — Char fallback**
/// If the input is not valid JSON or field compression is insufficient, fall back to
/// character-level truncation with a `…[truncated]` marker.
fn compress_tool_result(content: &str, budget: usize) -> String {
    const FIELD_VALUE_MAX: usize = 300;
    const MAX_ARRAY_ITEMS: usize = 50;

    // Stage 1: try JSON-aware compression.
    if let Ok(parsed) = serde_json::from_str::<Value>(content) {
        let compressed = compress_json_value(parsed, FIELD_VALUE_MAX);
        let serialized = compressed.to_string();

        if serialized.len() <= budget {
            return serialized;
        }

        // Still over budget — if it's an array, trim items.
        if let Ok(Value::Array(mut items)) = serde_json::from_str::<Value>(&serialized) {
            let total = items.len();
            // Binary-search for how many items fit within budget.
            let mut keep = 0usize;
            for i in 1..=items.len().min(MAX_ARRAY_ITEMS) {
                let candidate = serde_json::to_string(&items[..i]).unwrap_or_default();
                // Reserve ~30 chars for the __more__ sentinel.
                if candidate.len() + 30 > budget { break; }
                keep = i;
            }
            let remaining = total.saturating_sub(keep);
            items.truncate(keep);
            if remaining > 0 {
                items.push(json!({ "__more__": remaining }));
            }
            return serde_json::to_string(&Value::Array(items)).unwrap_or_else(|_| serialized);
        }
    }

    // Stage 2: char-level fallback.
    if content.len() <= budget {
        content.to_string()
    } else {
        format!("{}…[truncated]", &content[..budget])
    }
}

/// Recursively traverse a JSON value and truncate any string value longer than
/// `field_max` chars to `field_max` chars followed by `…`.
/// Object keys and non-string values are never modified.
fn compress_json_value(v: Value, field_max: usize) -> Value {
    match v {
        Value::String(s) => {
            if s.chars().count() > field_max {
                let truncated: String = s.chars().take(field_max).collect();
                Value::String(format!("{}…", truncated))
            } else {
                Value::String(s)
            }
        }
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(|item| compress_json_value(item, field_max)).collect())
        }
        Value::Object(map) => {
            Value::Object(
                map.into_iter()
                    .map(|(k, val)| (k, compress_json_value(val, field_max)))
                    .collect()
            )
        }
        other => other,
    }
}
