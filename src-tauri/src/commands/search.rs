use crate::{api_client::daemon_get, error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResult {
    pub path: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
    /// Original web URL, set when result comes from a knowledge import chunk (imports/ path)
    pub source_url: Option<String>,
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
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let vault_id = state.get_vault_id().await?;

    let rows: Vec<serde_json::Value> = daemon_get(
        &state.http_client,
        &format!(
            "/vaults/{}/search?q={}&limit={}",
            urlencoding::encode(&vault_id),
            urlencoding::encode(query.trim()),
            limit,
        ),
        tok,
    ).await.unwrap_or_default();

    Ok(rows.into_iter().map(|r| {
        let snippet = r["section"].as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| r["snippet"].as_str().unwrap_or(""))
            .chars().take(200).collect();
        let path = r["path"].as_str().unwrap_or("").to_string();
        let source_url = if path.starts_with("imports/") {
            r["source_url"].as_str().map(|s| s.to_string())
        } else {
            None
        };
        SearchResult {
            path,
            title: r["title"].as_str().unwrap_or("").to_string(),
            snippet,
            score: r["score"].as_f64().unwrap_or(0.0),
            source_url,
        }
    }).collect())
}
