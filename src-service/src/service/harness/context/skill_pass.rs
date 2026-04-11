//! Skill pre-pass: resolves active skills before context assembly.
//!
//! Runs semantic skill matching (or loads pinned skills) in parallel with the
//! memory prefetch. Merges newly matched skills with any cached from the previous
//! message so the skill chain persists across turns without re-running search.

use serde_json::Value;
use crate::db::SurrealDb;
use super::super::memory::working::WorkingMemory;
use super::super::tools::skill_tools::{self, SkillPassResult};

/// Output of the skill pre-pass, consumed by `build_agent_context`.
pub(crate) struct SkillPrePassOutput {
    /// Skill-injected content to append to the system message.
    pub system_injection:       String,
    /// Titles of skills activated this turn (used for SSE emit by caller).
    pub activated_skill_titles: Vec<String>,
    /// Tool name list updated with any tools contributed by activated skills.
    pub tool_names:             Vec<String>,
    /// Whether the previous message already had active skills (captured before
    /// this pass potentially calls `set_active_skills`).
    pub had_active_skills:      bool,
}

/// Run the skill pre-pass for one agent turn.
///
/// Determines which skills apply (via semantic search or pinned list), merges
/// them with skills carried over from the previous message, persists the result
/// in `working_memory`, and returns injection text + metadata.
///
/// The caller is responsible for emitting `agent:skills_activated` if streaming.
pub(crate) async fn run(
    db:             &SurrealDb,
    client:         &reqwest::Client,
    embedding_url:  &Option<String>,
    vault_id:       &str,
    account_id:     &str,
    agent_def:      &Value,
    working_memory: &WorkingMemory,
    input:          &str,
    mut tool_names: Vec<String>,
) -> SkillPrePassOutput {
    // Capture BEFORE any potential `set_active_skills` call below so it
    // reflects the previous message's state for context-loading decisions.
    let had_active_skills = working_memory.has_active_skills().await;

    let do_skill_pass = !vault_id.is_empty()
        && agent_def["use_skill_pass"].as_bool().unwrap_or(false);
    let pinned_skill_ids: Vec<String> = agent_def["skill_ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
        .unwrap_or_default();
    // use_pinned: agent has explicitly pinned skills AND skill_pass is disabled.
    let use_pinned = !agent_def["use_skill_pass"].as_bool().unwrap_or(false)
        && !pinned_skill_ids.is_empty();

    let cached_skill: Option<SkillPassResult> = if do_skill_pass {
        working_memory.clone_active_skills().await
    } else {
        None
    };

    let skill_result: Option<SkillPassResult> = if use_pinned {
        Some(skill_tools::load_skills_by_ids(db, &pinned_skill_ids).await)
    } else if do_skill_pass {
        let new_result = skill_tools::run_skill_pass(
            client, embedding_url, db, vault_id, account_id, input,
        ).await;
        if let Some(mut cached) = cached_skill {
            // Merge: carry forward sticky skills and append any newly matched ones.
            // This lets mid-conversation new requirements add skills without losing
            // skills that were matched in earlier rounds.
            for title in &new_result.skill_titles {
                if !cached.skill_titles.contains(title) {
                    cached.skill_titles.push(title.clone());
                }
            }
            for tool in &new_result.skill_tool_names {
                if !cached.skill_tool_names.contains(tool) {
                    cached.skill_tool_names.push(tool.clone());
                }
            }
            if !new_result.system_injection.is_empty() {
                cached.system_injection = if cached.system_injection.is_empty() {
                    new_result.system_injection
                } else {
                    format!("{}\n\n{}", cached.system_injection, new_result.system_injection)
                        .chars().take(2000).collect()
                };
            }
            Some(cached)
        } else {
            Some(new_result)
        }
    } else {
        None
    };

    let mut activated_skill_titles: Vec<String> = Vec::new();
    let system_injection = if let Some(skill) = skill_result {
        if skill.skill_titles.is_empty() {
            let discovery =
                skill_tools::build_skill_discovery_injection(db, account_id).await;
            if !tool_names.contains(&"create_agent_skill".to_string()) {
                tool_names.push("create_agent_skill".to_string());
            }
            discovery
        } else {
            tracing::info!(
                "[skill_pass] activated {} skills: {:?}",
                skill.skill_titles.len(),
                skill.skill_titles
            );
            activated_skill_titles = skill.skill_titles.clone();
            // Persist in WorkingMemory so subsequent ReAct rounds re-use the
            // same skills without re-running semantic search. Cleared when a
            // round produces no tool calls.
            if !working_memory.has_active_skills().await {
                working_memory.set_active_skills(skill.clone()).await;
            }
            for t in skill.skill_tool_names {
                if !tool_names.contains(&t) {
                    tool_names.push(t);
                }
            }
            skill.system_injection
        }
    } else {
        String::new()
    };

    SkillPrePassOutput {
        system_injection,
        activated_skill_titles,
        tool_names,
        had_active_skills,
    }
}
