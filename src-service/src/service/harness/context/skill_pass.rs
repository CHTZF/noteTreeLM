//! Skill pre-pass: resolves active skills before context assembly.

use serde_json::Value;
use crate::db::SurrealDb;
use super::super::tools::skill_tools::{self, SkillPassResult};

/// Output of the skill pre-pass, consumed by `build_agent_context`.
pub(crate) struct SkillPrePassOutput {
    /// Skill-injected content to be added to the LLM context as a transient directive.
    pub system_injection:       String,
    /// Titles of skills activated this turn (used for SSE emit by caller).
    pub activated_skill_titles: Vec<String>,
    /// Tools contributed by activated skills — to be merged into the tool schema.
    pub skill_tool_names:       Vec<String>,
}

/// Run the skill pre-pass for one agent turn.
///
/// - `use_skill_pass = true`  → semantic/keyword search via `run_skill_pass`
/// - `use_pinned = true`      → load skills directly by `skill_ids`
/// - otherwise                → no-op (empty output)
pub(crate) async fn run(
    db:             &SurrealDb,
    client:         &reqwest::Client,
    embedding_url:  &Option<String>,
    vault_id:       &str,
    account_id:     &str,
    agent_def:      &Value,
    input:          &str,
) -> SkillPrePassOutput {
    let do_skill_pass = !vault_id.is_empty()
        && agent_def["use_skill_pass"].as_bool().unwrap_or(false);
    let pinned_skill_ids: Vec<String> = agent_def["skill_ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
        .unwrap_or_default();
    let use_pinned = !agent_def["use_skill_pass"].as_bool().unwrap_or(false)
        && !pinned_skill_ids.is_empty();

    let skill_result: Option<SkillPassResult> = if use_pinned {
        Some(skill_tools::load_skills_by_ids(db, &pinned_skill_ids).await)
    } else if do_skill_pass {
        Some(skill_tools::run_skill_pass(
            client, embedding_url, db, vault_id, account_id, input,
        ).await)
    } else {
        None
    };

    let mut activated_skill_titles: Vec<String> = Vec::new();
    let mut skill_tool_names: Vec<String> = Vec::new();
    let system_injection = if let Some(skill) = skill_result {
        if skill.skill_titles.is_empty() {
            let discovery =
                skill_tools::build_skill_discovery_injection(db, account_id).await;
            skill_tool_names.push("create_agent_skill".to_string());
            discovery
        } else {
            tracing::info!(
                "[skill_pass] activated {} skills: {:?}",
                skill.skill_titles.len(),
                skill.skill_titles
            );
            activated_skill_titles = skill.skill_titles;
            skill_tool_names = skill.skill_tool_names;
            skill.system_injection
        }
    } else {
        String::new()
    };

    SkillPrePassOutput {
        system_injection,
        activated_skill_titles,
        skill_tool_names,
    }
}
