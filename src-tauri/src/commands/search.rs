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

    // FTS5 搜尋（標題權重較高）
    let rows = sqlx::query_as::<_, (String, String, f64)>(
        "SELECT s.path, s.title, bm25(search_fts) as score
         FROM search_fts s
         WHERE search_fts MATCH ?
         ORDER BY score
         LIMIT ?"
    )
    .bind(&query)
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
        let start = pos.saturating_sub(60);
        let end = (pos + query.len() + 60).min(content.len());
        let snippet = &content[start..end];
        format!("...{}...", snippet.trim())
    } else {
        content.chars().take(120).collect::<String>() + "..."
    }
}
