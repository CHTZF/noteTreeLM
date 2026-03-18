use crate::{error::AppError, state::AppState};
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

/// Extract `source: <url>` from YAML frontmatter if present
fn extract_source_url(content: &str) -> Option<String> {
    let body = content.strip_prefix("---")?;
    let end = body.find("---")?;
    body[..end].lines()
        .find_map(|line| line.strip_prefix("source:").map(|v| v.trim().to_string()))
        .filter(|s| !s.is_empty())
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

    // ── 向量搜尋（embedding server 執行中時）──────────────────────────
    let emb_url: Option<String> = {
        let port = *state.embedding_actual_port.lock().await;
        port.map(|p| format!("http://127.0.0.1:{}", p))
    };

    if let Some(ref base_url) = emb_url {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        let query_vec = crate::commands::ai::get_embedding(&client, base_url, query.trim()).await;

        if !query_vec.is_empty() {
            #[derive(Deserialize)]
            struct ChunkRow { file_path: String, content: String, score: f64 }

            let fetch_limit = limit * 3; // 多取再 dedup
            let mut resp = db.query(
                "SELECT file_path, section, content, \
                 vector::similarity::cosine(embedding, $vec) AS score \
                 FROM chunks WHERE vault_id = $vid AND embedding IS NOT NONE \
                 ORDER BY score DESC LIMIT $lim"
            )
            .bind(("vid", vault_id.clone()))
            .bind(("vec", query_vec))
            .bind(("lim", fetch_limit))
            .await.ok();

            let chunk_rows: Vec<ChunkRow> = resp.as_mut()
                .and_then(|r| r.take::<Vec<ChunkRow>>(0).ok())
                .unwrap_or_default();

            if !chunk_rows.is_empty() {
                // Dedup by file_path，每個檔案只保留分數最高的 chunk
                let mut seen = std::collections::HashSet::new();
                let mut deduped: Vec<ChunkRow> = Vec::new();
                for row in chunk_rows {
                    if seen.insert(row.file_path.clone()) {
                        deduped.push(row);
                        if deduped.len() >= limit as usize { break; }
                    }
                }

                // 批次取 title（一次 query）
                let paths: Vec<String> = deduped.iter().map(|r| r.file_path.clone()).collect();
                #[derive(Deserialize)]
                struct TitleRow { path: String, title: String }
                let mut tr = db.query(
                    "SELECT path, title FROM notes WHERE vault_id = $vid AND path IN $paths"
                )
                .bind(("vid", vault_id.clone()))
                .bind(("paths", paths))
                .await.ok();
                let title_rows: Vec<TitleRow> = tr.as_mut()
                    .and_then(|r| r.take::<Vec<TitleRow>>(0).ok())
                    .unwrap_or_default();
                let title_map: std::collections::HashMap<String, String> =
                    title_rows.into_iter().map(|r| (r.path, r.title)).collect();

                let results: Vec<SearchResult> = deduped.into_iter().map(|row| {
                    let title = title_map.get(&row.file_path)
                        .cloned()
                        .unwrap_or_else(|| {
                            row.file_path.split('/').last()
                                .unwrap_or(&row.file_path)
                                .trim_end_matches(".md")
                                .to_string()
                        });
                    let snippet = extract_snippet(&row.content, query.trim());
                    let source_url = if row.file_path.starts_with("imports/") {
                        extract_source_url(&row.content)
                    } else { None };
                    SearchResult { path: row.file_path, title, snippet, score: row.score, source_url }
                }).collect();

                return Ok(results);
            }
        }
    }

    // ── Fallback：FTS（embedding 不可用或無結果）──────────────────────
    #[derive(Deserialize)]
    struct SearchRow { path: String, title: String }

    let mut resp = db
        .query(
            "SELECT path, title FROM notes WHERE vault_id = $vid \
             AND (title @1@ $q OR content @2@ $q) \
             ORDER BY search::score(1) + search::score(2) DESC LIMIT $limit",
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
        struct ContentRow { content: String }

        let mut resp2 = db
            .query("SELECT content FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .bind(("path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let content_rows: Vec<ContentRow> = resp2.take(0).unwrap_or_default();
        let content = content_rows.into_iter().next().map(|r| r.content).unwrap_or_default();

        let snippet = extract_snippet(&content, query.trim());
        let score = 1.0 / (i as f64 + 1.0);

        results.push(SearchResult { path: row.path, title: row.title, snippet, score, source_url: None });
    }

    Ok(results)
}

fn extract_snippet(content: &str, query: &str) -> String {
    let query_lower = query.to_lowercase();
    let content_lower = content.to_lowercase();

    if let Some(pos) = content_lower.find(&query_lower) {
        let raw_start = pos.saturating_sub(60);
        let start = (0..=raw_start).rev().find(|&i| content.is_char_boundary(i)).unwrap_or(0);
        let raw_end = (pos + query_lower.len() + 60).min(content.len());
        let end = (raw_end..=content.len()).find(|&i| content.is_char_boundary(i)).unwrap_or(content.len());
        format!("...{}...", content[start..end].trim())
    } else {
        content.chars().take(120).collect::<String>() + "..."
    }
}
