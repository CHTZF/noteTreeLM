use std::collections::HashSet;
use super::super::tool_def::ALL_TOOL_DEFS;
use crate::service::types::NeedConfirmFn;

/// Write tools that are exempt from the path-existence guard requirement.
///
/// Creation tools operate on new/non-existent paths where path-existence pre-checks
/// don't apply. Memory-write tools target DB rows, not vault files.
/// Append any new creation-style or memory-write tools here to avoid the debug_assert.
pub(crate) const GUARD_EXEMPT_WRITE_TOOLS: &[&str] = &[
    "create_note",
    "create_folder",
    "create_agent_skill",
    "compress_to_knowledge",   // creates a new knowledge note, target path doesn't pre-exist
    "schedule_task",           // creates a new task note, target path doesn't pre-exist
    "save_memory_facts",
    "mark_conversation_processed",
    "condense_memory_facts",
];

/// Non-write tools that still require user confirmation before execution
/// (e.g. tools that call external services or have irreversible side effects).
const EXTRA_CONFIRM_TOOLS: &[&str] = &[
    "call_external_ai",
];

/// Build a confirmation predicate based on agent `kind`.
///
/// - `"background"` / `"sub"` / `"scheduled"` → never confirm (no frontend listener).
/// - `"chat"` or any unknown kind → default list: all `is_write` tools + `EXTRA_CONFIRM_TOOLS`.
///
/// New kinds can be handled here independently as requirements evolve.
pub(crate) fn build_need_confirm_fn(kind: &str) -> NeedConfirmFn {
    match kind {
        "background" | "sub" | "scheduled" => std::sync::Arc::new(|_| false),
        _ => {
            let mut set: HashSet<&'static str> = ALL_TOOL_DEFS.iter()
                .filter(|d| d.is_write)
                .map(|d| d.name)
                .collect();
            for &name in EXTRA_CONFIRM_TOOLS {
                set.insert(name);
            }
            std::sync::Arc::new(move |name: &str| set.contains(name))
        }
    }
}

/// Check that every destructive write tool has a guard spec or appears in
/// `GUARD_EXEMPT_WRITE_TOOLS`.
///
/// - All builds: logs a warning for each uncovered tool so production gaps are visible.
/// - Debug builds: additionally panics to catch omissions during development.
///
/// Call once during registry construction (e.g. `build_interactive_registry`).
pub(crate) fn assert_guard_coverage() {
    for def in ALL_TOOL_DEFS {
        let uncovered = def.is_write
            && def.guard.is_none()
            && !GUARD_EXEMPT_WRITE_TOOLS.contains(&def.name);
        if uncovered {
            tracing::warn!(
                "[guard_coverage] write tool '{}' has no guard spec and is not in GUARD_EXEMPT_WRITE_TOOLS",
                def.name
            );
            debug_assert!(false,
                "write tool '{}' has no guard spec and is not in GUARD_EXEMPT_WRITE_TOOLS",
                def.name
            );
        }
    }
}
