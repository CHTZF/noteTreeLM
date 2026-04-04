//! Context pipeline: assembles the message list sent to the LLM each turn.
//!
//! ## Slot model
//!
//! The context window is divided into three budget slots:
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
//!
//! Token estimation uses char count (1 char ≈ 1 token for CJK, slightly
//! pessimistic for ASCII — intentionally conservative).

use serde_json::{json, Value};
use crate::db::SurrealDb;
use super::super::engine::context::load_messages_db;

// ── Budget ────────────────────────────────────────────────────────────────────

/// Character-based budget for each context slot.
/// Tune these constants to match your model's context window.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ContextBudget {
    /// Max chars for the entire system message (base + injections + memory).
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
        Self {
            system_chars:  4_000,
            memory_chars:  1_200,
            history_chars: 12_000,
            keep_recent:   6,
        }
    }
}

// ── Input ─────────────────────────────────────────────────────────────────────

/// All inputs needed to assemble a context window for one LLM turn.
pub(crate) struct ContextInput<'a> {
    pub db:               &'a SurrealDb,
    pub conv_id:          &'a str,
    pub user_input:       &'a str,
    /// Base system prompt from agent_def.
    pub system_prompt:    &'a str,
    /// Optional free-text injected below the base prompt (from skill pass).
    pub skill_injection:  &'a str,
    /// Optional user activity context (recent UI events).
    pub activity_context: Option<&'a str>,
    /// Memory facts pre-fetched by the parallel pre-pass.
    pub memory_facts:     &'a [Value],
}

// ── Output ────────────────────────────────────────────────────────────────────

/// Result of running the context pipeline: the assembled message list + diagnostics.
/// Diagnostic fields (system_chars_used, history_chars_before_trim, was_trimmed) are
/// reserved for Phase 5 observability and not yet read in normal execution paths.
#[allow(dead_code)]
pub(crate) struct BuiltContext {
    /// Final message list ready to send to the LLM.
    pub messages: Vec<Value>,
    /// How many chars the system slot actually consumed.
    pub system_chars_used: usize,
    /// How many chars the history slot actually consumed (before trimming).
    pub history_chars_before_trim: usize,
    /// Whether history was trimmed this turn.
    pub was_trimmed: bool,
}

// ── Pipeline ──────────────────────────────────────────────────────────────────

pub(crate) struct ContextPipeline {
    pub budget: ContextBudget,
}

impl ContextPipeline {
    pub fn new(budget: ContextBudget) -> Self {
        Self { budget }
    }

    /// Assemble the full message list in three stages:
    /// 1. Build system message (base + anti-hallucination + activity + memory + skill injection)
    /// 2. Load + append conversation history
    /// 3. Trim history if over budget
    pub async fn build(
        &self,
        input: ContextInput<'_>,
        client: &reqwest::Client,
        llm_url: &str,
    ) -> BuiltContext {
        // ── Stage 1: System message ────────────────────────────────────────
        let system_content = self.build_system_content(&input);
        let system_chars_used = system_content.len();

        // ── Stage 2: History ───────────────────────────────────────────────
        let mut history = load_messages_db(input.db, input.conv_id).await;

        // Ensure current user input is the last message.
        if history.last().and_then(|m| m["role"].as_str()) != Some("user") {
            history.push(json!({"role": "user", "content": input.user_input}));
        }

        let history_chars_before_trim: usize = history.iter()
            .map(|m| m["content"].as_str().unwrap_or("").len())
            .sum();

        // ── Stage 3: Trim history if over budget ───────────────────────────
        let (history, was_trimmed) = self.trim_history(history, client, llm_url).await;

        // Assemble: system first, then history (history already ends with user turn).
        let mut messages = Vec::with_capacity(1 + history.len());
        messages.push(json!({"role": "system", "content": system_content}));
        messages.extend(history);

        BuiltContext {
            messages,
            system_chars_used,
            history_chars_before_trim,
            was_trimmed,
        }
    }

    // ── Stage 1 helper ────────────────────────────────────────────────────────

    fn build_system_content(&self, input: &ContextInput<'_>) -> String {
        // Anti-hallucination suffix appended to every system prompt.
        const ANTI_HALLUCINATION: &str =
            "\n\n必須實際呼叫工具完成任務；禁止假裝或虛構結果。\
             若搜尋無結果，直接說明找不到。\
             回覆中引用筆記時，請包含完整的 vault 相對路徑。\
             工具結果中含有 __cite_id__ 欄位；最終文字回覆的第一句必須以 \
             [cite:id1,id2] 格式引用所依據的工具結果，若本輪未使用任何工具則輸出 [cite:none]。";

        let mut parts: Vec<String> = Vec::new();

        // 1a. Base system prompt
        parts.push(input.system_prompt.to_string());

        // 1b. Activity context (user's recent UI events)
        if let Some(ac) = input.activity_context {
            if !ac.is_empty() {
                parts.push(format!("[使用者活動紀錄]\n{}", ac));
            }
        }

        // 1c. Anti-hallucination (always appended right after base content)
        parts.push(ANTI_HALLUCINATION.to_string());

        // 1d. Memory facts (capped by memory_chars budget)
        if !input.memory_facts.is_empty() {
            let mem_block = self.build_memory_block(input.memory_facts);
            parts.push(mem_block);
        }

        // 1e. Skill injection (capped by remaining system budget)
        if !input.skill_injection.is_empty() {
            let so_far: usize = parts.iter().map(|s| s.len()).sum();
            let remaining = self.budget.system_chars.saturating_sub(so_far);
            if remaining > 50 {
                let capped: String = input.skill_injection.chars()
                    .take(remaining)
                    .collect();
                parts.push(capped);
            }
        }

        parts.join("\n\n")
    }

    fn build_memory_block(&self, facts: &[Value]) -> String {
        let lines: String = facts.iter()
            .map(|f| format!("[{}] {}",
                f["category"].as_str().unwrap_or("general"),
                f["content"].as_str().unwrap_or("")
            ))
            .collect::<Vec<_>>()
            .join("\n");

        // Cap memory block at memory_chars budget.
        let block = format!("## 相關記憶\n{}", lines);
        block.chars().take(self.budget.memory_chars).collect()
    }

    // ── Stage 3 helper ────────────────────────────────────────────────────────

    /// Summarise oldest turns when history exceeds budget.
    /// Returns (trimmed_history, was_trimmed).
    async fn trim_history(
        &self,
        hist: Vec<Value>,
        client: &reqwest::Client,
        llm_url: &str,
    ) -> (Vec<Value>, bool) {
        let total: usize = hist.iter()
            .map(|m| m["content"].as_str().unwrap_or("").len())
            .sum();

        if total <= self.budget.history_chars || hist.len() <= self.budget.keep_recent {
            return (hist, false);
        }

        let keep_from = hist.len().saturating_sub(self.budget.keep_recent);
        let old_text: String = hist[..keep_from].iter().map(|m| {
            let role    = m["role"].as_str().unwrap_or("user");
            let content = m["content"].as_str().unwrap_or("");
            format!("[{}]: {}", role, &content[..content.len().min(500)])
        }).collect::<Vec<_>>().join("\n\n");
        let recent = hist[keep_from..].to_vec();

        let summary = summarise_with_llm(client, llm_url, &old_text).await;
        let trimmed = if summary.is_empty() {
            recent
        } else {
            let mut v = vec![json!({"role": "assistant", "content": format!("[對話摘要]\n{}", summary)})];
            v.extend(recent);
            v
        };
        (trimmed, true)
    }
}

// ── LLM summariser (extracted from trim_context) ─────────────────────────────

async fn summarise_with_llm(
    client: &reqwest::Client,
    llm_url: &str,
    old_text: &str,
) -> String {
    let result: Option<String> = async {
        let resp = client
            .post(format!("{}/v1/chat/completions", llm_url))
            .json(&json!({
                "messages": [
                    {"role": "system", "content": "你是對話摘要助手。請將以下對話歷史壓縮為 2-5 句重點摘要，保留關鍵需求、決策和上下文。用繁體中文回答，不要加任何前綴。"},
                    {"role": "user",   "content": old_text},
                ],
                "stream": false,
                "temperature": 0.1,
                "max_tokens": 400,
            }))
            .send().await.ok()?;
        if !resp.status().is_success() { return None; }
        let j: Value = resp.json().await.ok()?;
        j["choices"][0]["message"]["content"].as_str().map(String::from)
    }.await;
    result.unwrap_or_default()
}
