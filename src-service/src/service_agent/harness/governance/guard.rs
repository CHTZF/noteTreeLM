use serde_json::Value;
use crate::state::GuardHint;

// ── Guard types ───────────────────────────────────────────────────────────────


/// Evidence tier required before a guarded write tool may execute.
#[derive(Copy, Clone)]
pub(crate) enum GuardLevel {
    /// Path must appear in a prior tool's result (search/list result array, or read/open args).
    PathSeen,
    /// Additionally, read_note must have returned non-error content for this exact path.
    ContentRead,
}

/// Declarative precondition spec. All fields are fn pointers → ToolDef stays Copy.
#[derive(Copy, Clone)]
pub(crate) struct ToolGuardSpec {
    /// Extract the target path string from the tool's args.
    pub path_extractor: fn(&Value) -> String,
    /// Minimum evidence level required.
    pub require: GuardLevel,
    /// If true, target is a folder (skip .md normalisation; match "folder/" in list_structure text).
    pub is_folder: bool,
}

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Ensure a path has a .md suffix (lowercase-first to avoid "FOO.MD" → "foo.md.md").
#[inline]
pub(crate) fn norm_path(p: &str) -> String {
    let lower = p.to_lowercase();
    if lower.ends_with(".md") { lower } else { format!("{}.md", lower) }
}

/// Validate that a vault-relative path is safe: non-empty, no `..` traversal, no `.` components,
/// and not an absolute path. Applied to all write tools before any filesystem access.
pub(crate) fn validate_rel_path(rel_path: &str) -> Result<(), String> {
    if rel_path.is_empty() {
        return Err("路徑不能為空".to_string());
    }
    for component in std::path::Path::new(rel_path).components() {
        match component {
            std::path::Component::ParentDir =>
                return Err(format!("路徑不允許包含 '..'：{}", rel_path)),
            std::path::Component::CurDir =>
                return Err(format!("路徑不允許包含 '.'：{}", rel_path)),
            std::path::Component::RootDir | std::path::Component::Prefix(_) =>
                return Err(format!("路徑必須是相對路徑：{}", rel_path)),
            _ => {}
        }
    }
    Ok(())
}

// ── Store types and query helpers ─────────────────────────────────────────────

pub(crate) type StoreMap = std::collections::HashMap<String, crate::state::ToolCallRecord>;

/// Returns true if a read_note result value indicates a failure (file not found, empty vault, etc.).
/// vault_tools::vault_read_note always returns Ok(String), so errors are encoded as specific prefixes.
pub(crate) fn is_read_note_error(result: &Value) -> bool {
    match result.as_str() {
        Some(s) => s.starts_with("讀取失敗：") || s == "Vault 未設定" || s == "路徑為空" || s.is_empty(),
        None => true,
    }
}

/// Check whether `target` (already normalized) appears in any prior tool's evidence.
/// `is_folder`: skip note-only sources and use text substring for list_structure.
pub(crate) fn check_path_seen(store: &StoreMap, target: &str, is_folder: bool) -> bool {
    store.values().any(|rec| match rec.name.as_str() {
        "search_vault" => {
            !is_folder && rec.result.as_array().map(|a| a.iter().any(|r|
                r["path"].as_str().map(|p| norm_path(p) == target).unwrap_or(false)
            )).unwrap_or(false)
        }
        "list_structure" => {
            rec.result.as_str().map(|text| {
                let text_lower = text.to_lowercase();
                if is_folder {
                    text_lower.contains(&format!("{}/", target))
                } else {
                    text_lower.contains(target)
                }
            }).unwrap_or(false)
        }
        "read_note" => {
            !is_folder
            && rec.args["path"].as_str().map(|p| norm_path(p) == target).unwrap_or(false)
            && !is_read_note_error(&rec.result)
        }
        "open_note" => {
            !is_folder && (
                rec.args["path"].as_str().map(|p| norm_path(p) == target).unwrap_or(false)
                || rec.args["paths"].as_array().map(|a| a.iter().any(|p|
                    p.as_str().map(|p| norm_path(p) == target).unwrap_or(false)
                )).unwrap_or(false)
            )
        }
        _ => false,
    })
}

/// Check whether read_note succeeded for `target` (non-error content exists in store).
pub(crate) fn check_content_read(store: &StoreMap, target: &str) -> bool {
    store.values().any(|rec|
        rec.name == "read_note"
        && rec.args["path"].as_str().map(|p| norm_path(p) == target).unwrap_or(false)
        && !is_read_note_error(&rec.result)
    )
}

/// Check whether a relevant discovery tool was called (to give better error hints).
/// For folder guards only `list_structure` counts; for note guards either counts.
pub(crate) fn has_search_result(store: &StoreMap, is_folder: bool) -> bool {
    store.values().any(|rec| {
        if is_folder {
            rec.name == "list_structure"
        } else {
            matches!(rec.name.as_str(), "search_vault" | "list_structure")
        }
    })
}

// ── Guard evaluation ──────────────────────────────────────────────────────────

/// Evaluate a `ToolGuardSpec` against the current tool evidence store.
/// Returns `None` if the guard passes (execution may proceed).
/// Returns `Some(GuardHint)` with a structured block reason if blocked.
pub(crate) fn evaluate_guard(spec: &ToolGuardSpec, args: &Value, store: &StoreMap) -> Option<GuardHint> {
    let raw_path = (spec.path_extractor)(args);
    if raw_path.is_empty() {
        return Some(GuardHint::new("路徑參數不能為空，請提供有效的路徑後再試。"));
    }
    let target = if spec.is_folder {
        raw_path.to_lowercase()
    } else {
        norm_path(&raw_path)
    };

    let path_ok    = check_path_seen(store, &target, spec.is_folder);
    let content_ok = !matches!(spec.require, GuardLevel::ContentRead)
        || check_content_read(store, &target);
    let was_searched = has_search_result(store, spec.is_folder);

    if !path_ok {
        let hint = if spec.is_folder {
            if was_searched {
                GuardHint::new(format!("list_structure 結果中找不到資料夾 '{}'，請確認名稱是否正確。", raw_path))
                    .with_tool("list_structure", &raw_path)
            } else {
                GuardHint::new(format!("資料夾 '{}' 尚未驗證存在，請先呼叫 list_structure 確認。", raw_path))
                    .with_tool("list_structure", &raw_path)
            }
        } else if was_searched {
            GuardHint::new(format!("搜尋結果中找不到 '{}'，請確認筆記名稱或換個關鍵字再搜尋。", raw_path))
                .with_tool("search_vault", &raw_path)
        } else {
            GuardHint::new(format!("路徑 '{}' 尚未驗證存在，請先使用 search_vault 或 list_structure 確認。", raw_path))
                .with_tool("search_vault", &raw_path)
        };
        return Some(hint);
    }
    if !content_ok {
        return Some(
            GuardHint::new(format!(
                "尚未成功讀取 '{}' 的內容（讀取失敗或未呼叫 read_note）。請先呼叫 read_note 確認內容後再修改。",
                raw_path
            ))
            .with_tool("read_note", &raw_path)
        );
    }
    None
}

// ── Layer 1: Guard regression tests ──────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use crate::state::{GuardOutcome, ToolCallRecord};

    // ── Test helpers ─────────────────────────────────────────────────────────

    fn make_store(records: Vec<ToolCallRecord>) -> StoreMap {
        records.into_iter().enumerate()
            .map(|(i, r)| (format!("call_{}", i), r))
            .collect()
    }

    fn rec(name: &str, args: serde_json::Value, result: serde_json::Value) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            args,
            result,
            started_at:    0,
            duration_ms:   0,
            guard_outcome: GuardOutcome::Passed,
        }
    }

    // Guard spec: note path — PathSeen level.
    const GUARD_PATH: ToolGuardSpec = ToolGuardSpec {
        path_extractor: |args| args["path"].as_str().unwrap_or("").to_string(),
        require:        GuardLevel::PathSeen,
        is_folder:      false,
    };

    // Guard spec: note path — ContentRead level.
    const GUARD_CONTENT: ToolGuardSpec = ToolGuardSpec {
        path_extractor: |args| args["path"].as_str().unwrap_or("").to_string(),
        require:        GuardLevel::ContentRead,
        is_folder:      false,
    };

    // Guard spec: folder path.
    const GUARD_FOLDER: ToolGuardSpec = ToolGuardSpec {
        path_extractor: |args| args["path"].as_str().unwrap_or("").to_string(),
        require:        GuardLevel::PathSeen,
        is_folder:      true,
    };

    // ── Test cases ────────────────────────────────────────────────────────────

    /// Empty path → always blocked with "路徑不能為空" hint.
    #[test]
    fn empty_path_is_blocked() {
        let store = make_store(vec![]);
        let result = evaluate_guard(&GUARD_PATH, &json!({"path": ""}), &store);
        assert!(result.is_some());
        assert!(result.unwrap().message.contains("不能為空"));
    }

    /// No relevant prior tool calls → blocked.
    #[test]
    fn empty_store_blocks() {
        let store = make_store(vec![]);
        let result = evaluate_guard(&GUARD_PATH, &json!({"path": "notes/foo.md"}), &store);
        assert!(result.is_some());
    }

    /// search_vault result containing the target path → PathSeen passes.
    #[test]
    fn search_vault_path_seen_passes() {
        let store = make_store(vec![rec(
            "search_vault",
            json!({"query": "foo"}),
            json!([{"path": "notes/foo.md", "content": "…"}]),
        )]);
        let result = evaluate_guard(&GUARD_PATH, &json!({"path": "notes/foo.md"}), &store);
        assert!(result.is_none(), "should pass but got: {:?}", result);
    }

    /// search_vault result with DIFFERENT path → blocked.
    #[test]
    fn search_vault_wrong_path_still_blocked() {
        let store = make_store(vec![rec(
            "search_vault",
            json!({"query": "foo"}),
            json!([{"path": "notes/other.md", "content": "…"}]),
        )]);
        let result = evaluate_guard(&GUARD_PATH, &json!({"path": "notes/foo.md"}), &store);
        assert!(result.is_some());
    }

    /// list_structure text containing the path → PathSeen passes.
    #[test]
    fn list_structure_path_seen_passes() {
        let store = make_store(vec![rec(
            "list_structure",
            json!({}),
            json!("notes/\n  notes/foo.md\n  notes/bar.md"),
        )]);
        let result = evaluate_guard(&GUARD_PATH, &json!({"path": "notes/foo.md"}), &store);
        assert!(result.is_none(), "should pass but got: {:?}", result);
    }

    /// Successful read_note for path → PathSeen passes.
    #[test]
    fn read_note_path_seen_passes() {
        let store = make_store(vec![rec(
            "read_note",
            json!({"path": "notes/foo.md"}),
            json!("# Foo\nsome content"),
        )]);
        let result = evaluate_guard(&GUARD_PATH, &json!({"path": "notes/foo.md"}), &store);
        assert!(result.is_none(), "should pass but got: {:?}", result);
    }

    /// read_note returning an error string → does NOT count as PathSeen.
    #[test]
    fn read_note_error_does_not_count_as_path_seen() {
        let store = make_store(vec![rec(
            "read_note",
            json!({"path": "notes/foo.md"}),
            json!("讀取失敗：file not found"),
        )]);
        let result = evaluate_guard(&GUARD_PATH, &json!({"path": "notes/foo.md"}), &store);
        assert!(result.is_some(), "error read_note should not satisfy path guard");
    }

    /// ContentRead guard: only search_vault (path seen) but no successful read_note → blocked.
    #[test]
    fn content_guard_blocks_when_only_search_found() {
        let store = make_store(vec![rec(
            "search_vault",
            json!({"query": "foo"}),
            json!([{"path": "notes/foo.md"}]),
        )]);
        let result = evaluate_guard(&GUARD_CONTENT, &json!({"path": "notes/foo.md"}), &store);
        assert!(result.is_some());
        let hint = result.unwrap();
        assert!(hint.message.contains("read_note"), "hint should mention read_note, got: {}", hint.message);
        assert_eq!(hint.required_tool.as_deref(), Some("read_note"));
    }

    /// ContentRead guard: successful read_note → passes.
    #[test]
    fn content_guard_passes_with_successful_read_note() {
        let store = make_store(vec![rec(
            "read_note",
            json!({"path": "notes/foo.md"}),
            json!("# Foo\ncontent here"),
        )]);
        let result = evaluate_guard(&GUARD_CONTENT, &json!({"path": "notes/foo.md"}), &store);
        assert!(result.is_none(), "should pass but got: {:?}", result);
    }

    /// Folder guard: list_structure text contains "archive/" → PathSeen passes.
    #[test]
    fn folder_guard_passes_via_list_structure() {
        let store = make_store(vec![rec(
            "list_structure",
            json!({}),
            json!("vault/\n  archive/\n    archive/old.md"),
        )]);
        let result = evaluate_guard(&GUARD_FOLDER, &json!({"path": "archive"}), &store);
        assert!(result.is_none(), "should pass but got: {:?}", result);
    }

    /// Folder guard: list_structure text does NOT contain "missingfolder/" → blocked.
    #[test]
    fn folder_guard_blocks_when_not_in_list_structure() {
        let store = make_store(vec![rec(
            "list_structure",
            json!({}),
            json!("vault/\n  archive/\n    archive/old.md"),
        )]);
        let result = evaluate_guard(&GUARD_FOLDER, &json!({"path": "missing_folder"}), &store);
        assert!(result.is_some());
    }

    /// Path with .md suffix and without are treated as equivalent after norm_path.
    #[test]
    fn path_normalisation_is_case_insensitive() {
        let store = make_store(vec![rec(
            "read_note",
            json!({"path": "Notes/Foo.MD"}),  // uppercase
            json!("content"),
        )]);
        // Query with lowercase + no extension → after norm_path both become "notes/foo.md"
        let result = evaluate_guard(&GUARD_PATH, &json!({"path": "notes/foo"}), &store);
        assert!(result.is_none(), "should pass after norm_path, got: {:?}", result);
    }
}
