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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    #[derive(Deserialize)]
    struct SearchRow {
        path: String,
        title: String,
    }

    let mut resp = db
        .query(
            "SELECT path, title FROM notes WHERE vault_id = $vid AND (title @1@ $q OR content @2@ $q) ORDER BY search::score(1) + search::score(2) DESC LIMIT $limit",
        )
        .bind(("vid", vault_id.clone()))
        .bind(("q", query.trim().to_owned()))
        .bind(("limit", limit))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let rows: Vec<SearchRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    let mut results = Vec::new();
    for (i, row) in rows.into_iter().enumerate() {
        #[derive(Deserialize)]
        struct ContentRow {
            content: String,
        }

        let mut resp2 = db
            .query("SELECT content FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .bind(("path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let content_rows: Vec<ContentRow> = resp2.take(0).unwrap_or_default();
        let content = content_rows
            .into_iter()
            .next()
            .map(|r| r.content)
            .unwrap_or_default();

        let snippet = extract_snippet(&content, &query);
        // SurrealDB does not expose a raw BM25 score; use reverse rank as a proxy
        let score = 1.0 / (i as f64 + 1.0);

        results.push(SearchResult {
            path: row.path,
            title: row.title,
            snippet,
            score,
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
