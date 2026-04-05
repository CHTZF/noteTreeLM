use super::super::tool_def::ALL_TOOL_DEFS;

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

/// Returns true if `name` is a write tool (i.e., modifies vault/DB state and requires
/// user confirmation). Delegates to the ToolDef registry — single source of truth.
pub(crate) fn is_interactive_write_tool(name: &str) -> bool {
    super::super::tool_def::find_tool_def(name)
        .map(|d| d.is_write)
        .unwrap_or(false)
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
