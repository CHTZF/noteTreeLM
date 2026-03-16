pub mod chunker;
pub mod indexer;
pub mod watcher;

use std::path::{Path, PathBuf};

/// 將絕對路徑轉換為相對於 vault root 的相對路徑
pub fn to_relative_path(vault_root: &str, abs_path: &Path) -> Option<String> {
    let vault = PathBuf::from(vault_root);
    abs_path
        .strip_prefix(&vault)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

/// 將相對路徑轉換為絕對路徑
pub fn to_absolute_path(vault_root: &str, rel_path: &str) -> PathBuf {
    PathBuf::from(vault_root).join(rel_path)
}

/// 從路徑提取筆記標題（優先 H1，否則用檔名）
pub fn extract_title(path: &str, content: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("# ") {
            return trimmed[2..].trim().to_string();
        }
    }
    // fallback：使用檔名（不含副檔名）
    Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Untitled".to_string())
}

/// 計算文字字數（支援中文）
pub fn count_words(content: &str) -> i64 {
    let without_frontmatter = strip_frontmatter(content);
    without_frontmatter.split_whitespace().count() as i64
}

/// 去除 YAML frontmatter
pub fn strip_frontmatter(content: &str) -> &str {
    if content.starts_with("---") {
        if let Some(end) = content[3..].find("\n---") {
            return &content[3 + end + 4..];
        }
    }
    content
}
