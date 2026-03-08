use crate::{error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResult {
    pub path: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
}

#[tauri::command]
pub async fn search(
    state: State<'_, AppState>,
    query: String,
    limit: Option<i64>,
) -> Result<Vec<SearchResult>, AppError> {
    if query.trim().is_empty() {
        return Ok(vec![]);
    }

    let limit = limit.unwrap_or(20);
    let db = state.get_vault_db().await?;

    // FTS5 搜尋：每個空白分隔的詞用雙引號包起來，避免特殊字元造成語法錯誤，並加 * 做前綴比對
    let fts_query = query
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ");

    if fts_query.is_empty() {
        return Ok(vec![]);
    }

    let rows = sqlx::query_as::<_, (String, String, f64)>(
        "SELECT s.path, s.title, bm25(search_fts) as score
         FROM search_fts s
         WHERE search_fts MATCH ?
         ORDER BY score
         LIMIT ?"
    )
    .bind(&fts_query)
    .bind(limit)
    .fetch_all(&db)
    .await
    .unwrap_or_default();

    let mut results = Vec::new();
    for (path, title, score) in rows {
        // 取得內容片段
        let content: String = sqlx::query_scalar(
            "SELECT content FROM notes WHERE path = ?"
        )
        .bind(&path)
        .fetch_optional(&db)
        .await?
        .unwrap_or_default();

        let snippet = extract_snippet(&content, &query);

        results.push(SearchResult {
            path,
            title,
            snippet,
            score: score.abs(),
        });
    }

    Ok(results)
}

fn extract_snippet(content: &str, query: &str) -> String {
    let query_lower = query.to_lowercase();
    let content_lower = content.to_lowercase();

    if let Some(pos) = content_lower.find(&query_lower) {
        // 必須對齊 UTF-8 字元邊界，否則中文字（3 bytes）切到一半會 panic
        let raw_start = pos.saturating_sub(60);
        let start = (0..=raw_start).rev().find(|&i| content.is_char_boundary(i)).unwrap_or(0);
        let raw_end = (pos + query_lower.len() + 60).min(content.len());
        let end = (raw_end..=content.len()).find(|&i| content.is_char_boundary(i)).unwrap_or(content.len());
        format!("...{}...", content[start..end].trim())
    } else {
        content.chars().take(120).collect::<String>() + "..."
    }
}
