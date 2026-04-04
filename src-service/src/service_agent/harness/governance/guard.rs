use serde_json::Value;

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
/// Returns `Some(hint)` with a user-facing message if blocked.
pub(crate) fn evaluate_guard(spec: &ToolGuardSpec, args: &Value, store: &StoreMap) -> Option<String> {
    let raw_path = (spec.path_extractor)(args);
    if raw_path.is_empty() {
        return Some("路徑參數不能為空，請提供有效的路徑後再試。".to_string());
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
                format!("list_structure 結果中找不到資料夾 '{}'，請確認名稱是否正確。", raw_path)
            } else {
                format!("資料夾 '{}' 尚未驗證存在，請先呼叫 list_structure 確認。", raw_path)
            }
        } else if was_searched {
            format!("搜尋結果中找不到 '{}'，請確認筆記名稱或換個關鍵字再搜尋。", raw_path)
        } else {
            format!("路徑 '{}' 尚未驗證存在，請先使用 search_vault 或 list_structure 確認。", raw_path)
        };
        return Some(hint);
    }
    if !content_ok {
        return Some(format!(
            "尚未成功讀取 '{}' 的內容（讀取失敗或未呼叫 read_note）。請先呼叫 read_note 確認內容後再修改。",
            raw_path
        ));
    }
    None
}
