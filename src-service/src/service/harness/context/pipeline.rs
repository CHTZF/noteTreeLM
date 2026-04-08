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
    pub vault_id:         &'a str,
    pub user_input:       &'a str,
    /// Base system prompt from agent_def.
    pub system_prompt:    &'a str,
    /// Optional free-text injected below the base prompt (from skill pass).
    pub skill_injection:  &'a str,
    /// Optional user activity context (recent UI events).
    pub activity_context: Option<&'a str>,
    /// Memory facts pre-fetched by the parallel pre-pass.
    pub memory_facts:     &'a [Value],
    /// True for interactive chat agents (kind = "chat").
    pub is_chat:          bool,
    /// UI language for localised system messages.
    pub locale:           Locale,
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
        let mut history = EpisodicMemory::new(input.db.clone()).load(input.conv_id).await;
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

        // ── Stage 4: Injected system messages ─────────────────────────────
        let locale = input.locale;
        let (exec_history, checkpoint, agent_knowledge) = tokio::join!(
            load_exec_history(input.db, input.conv_id),
            load_checkpoint(input.db, input.conv_id, locale),
            load_agent_knowledge(input.db, input.vault_id, locale),
        );

        // Assemble: system → exec history → checkpoint → agent knowledge → history
        let mut messages = Vec::with_capacity(4 + history.len());
        messages.push(json!({"role": "system", "content": system_content}));
        if let Some(c) = exec_history  { messages.push(json!({"role": "system", "content": c})); }
        if let Some(c) = checkpoint    { messages.push(json!({"role": "system", "content": c})); }
        if let Some(c) = agent_knowledge { messages.push(json!({"role": "system", "content": c})); }
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

        if !input.skill_injection.is_empty() {
            let so_far: usize = parts.iter().map(|s| s.len()).sum();
            let remaining = self.budget.system_chars.saturating_sub(so_far);
            if remaining > 50 {
                let full_len = input.skill_injection.chars().count();
                let capped: String = input.skill_injection.chars().take(remaining).collect();
                if capped.len() < full_len {
                    tracing::warn!(
                        "[context] skill injection truncated: {}/{} chars kept (budget={})",
                        capped.len(), full_len, self.budget.system_chars
                    );
                }
                parts.push(capped);
            } else {
                tracing::warn!(
                    "[context] skill injection dropped: no budget (so_far={} budget={})",
                    so_far, self.budget.system_chars
                );
            }
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
