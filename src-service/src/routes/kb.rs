use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/vaults/:vault_id/kb/sessions",
            get(list_import_sessions).post(create_import_session),
        )
        .route(
            "/vaults/:vault_id/kb/sessions/:session_id",
            get(get_import_session)
                .patch(update_import_session)
                .delete(delete_import_session),
        )
        .route(
            "/vaults/:vault_id/kb/sessions/:session_id/pages",
            get(list_import_pages).post(add_import_pages),
        )
        .route(
            "/vaults/:vault_id/kb/sessions/:session_id/pages/:page_id",
            patch(update_import_page),
        )
        .route(
            "/vaults/:vault_id/kb/suggestions",
            get(list_kb_suggestions).post(create_kb_suggestion),
        )
        .route(
            "/vaults/:vault_id/kb/suggestions/:suggestion_id",
            delete(delete_kb_suggestion),
        )
        .route("/vaults/:vault_id/kb/stats", get(get_kb_stats))
        // Cross-session page search (no session_id in path)
        .route("/vaults/:vault_id/kb/pages", get(search_all_pages))
        .route(
            "/vaults/:vault_id/kb/items",
            get(list_knowledge_items).post(create_knowledge_item),
        )
        .route(
            "/vaults/:vault_id/kb/items/:item_id",
            get(get_knowledge_item)
                .put(update_knowledge_item)
                .patch(update_knowledge_item)
                .delete(delete_knowledge_item),
        )
}

async fn list_import_sessions(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM import_sessions WHERE vault_id = $vid ORDER BY created_at DESC")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Augment with page counts
    let mut summaries = Vec::new();
    for row in rows {
        let sid = row["session_id"].as_str().unwrap_or("").to_string();
        #[derive(serde::Deserialize)]
        struct CountRow { count: i64 }

        let mut cr = state.db
            .query("SELECT count() AS count FROM import_pages WHERE vault_id = $vid AND session_id = $sid GROUP ALL")
            .bind(("vid", vault_id.clone()))
            .bind(("sid", sid.clone()))
            .await.ok();
        let total_pages: i64 = cr.as_mut()
            .and_then(|r| r.take::<Vec<CountRow>>(0).ok())
            .and_then(|v| v.into_iter().next().map(|r| r.count))
            .unwrap_or(0);

        let mut ir = state.db
            .query("SELECT count() AS count FROM import_pages WHERE vault_id = $vid AND session_id = $sid AND status = 'imported' GROUP ALL")
            .bind(("vid", vault_id.clone()))
            .bind(("sid", sid))
            .await.ok();
        let imported_pages: i64 = ir.as_mut()
            .and_then(|r| r.take::<Vec<CountRow>>(0).ok())
            .and_then(|v| v.into_iter().next().map(|r| r.count))
            .unwrap_or(0);

        let mut summary = row.clone();
        if let Some(obj) = summary.as_object_mut() {
            obj.insert("total_pages".to_string(), json!(total_pages));
            obj.insert("imported_pages".to_string(), json!(imported_pages));
        }
        summaries.push(summary);
    }

    Ok(Json(json!(summaries)))
}

async fn create_import_session(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let session_id = Uuid::new_v4().to_string();
    let seed_url = body
        .get("seed_url")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing seed_url".to_string()))?
        .to_string();
    let site_name = body
        .get("site_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let conversation_id = body
        .get("conversation_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let root_folder = body
        .get("root_folder")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO import_sessions (vault_id, session_id, conversation_id, seed_url, site_name, root_folder, status, created_at, updated_at) VALUES ($vid, $sid, $conv, $url, $sname, $rfolder, 'pending', $now, $now)")
        .bind(("vid", vault_id))
        .bind(("sid", session_id.clone()))
        .bind(("conv", conversation_id))
        .bind(("url", seed_url))
        .bind(("sname", site_name))
        .bind(("rfolder", root_folder))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "session_id": session_id })))
}

async fn get_import_session(
    State(state): State<ApiState>,
    Path((vault_id, session_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM import_sessions WHERE vault_id = $vid AND session_id = $sid LIMIT 1")
        .bind(("vid", vault_id))
        .bind(("sid", session_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match rows.into_iter().next() {
        Some(row) => Ok(Json(row)),
        None => Err((StatusCode::NOT_FOUND, "Import session not found".to_string())),
    }
}

async fn update_import_session(
    State(state): State<ApiState>,
    Path((vault_id, session_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    if let Some(auto_update) = body.get("auto_update").and_then(|v| v.as_bool()) {
        state.db
            .query("UPDATE import_sessions SET auto_update = $au, updated_at = $now WHERE vault_id = $vid AND session_id = $sid")
            .bind(("au", auto_update))
            .bind(("now", now))
            .bind(("vid", vault_id))
            .bind(("sid", session_id))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn delete_import_session(
    State(state): State<ApiState>,
    Path((vault_id, session_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state.db
        .query("DELETE FROM import_pages WHERE vault_id = $vid AND session_id = $sid")
        .bind(("vid", vault_id.clone()))
        .bind(("sid", session_id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.db
        .query("DELETE FROM kb_suggestions WHERE vault_id = $vid AND session_id = $sid")
        .bind(("vid", vault_id.clone()))
        .bind(("sid", session_id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.db
        .query("DELETE FROM import_sessions WHERE vault_id = $vid AND session_id = $sid")
        .bind(("vid", vault_id))
        .bind(("sid", session_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize, Default)]
struct PagesQuery {
    status: Option<String>,
    q: Option<String>,
}

async fn list_import_pages(
    State(state): State<ApiState>,
    Path((vault_id, session_id)): Path<(String, String)>,
    Query(params): Query<PagesQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut query_str = "SELECT * FROM import_pages WHERE vault_id = $vid AND session_id = $sid".to_string();
    if params.status.is_some() { query_str.push_str(" AND status = $status"); }
    if params.q.is_some() {
        // FTS: match title or url (basic LIKE for SurrealDB)
        query_str.push_str(" AND (string::lowercase(title) CONTAINS string::lowercase($q) OR string::lowercase(url) CONTAINS string::lowercase($q))");
    }
    query_str.push_str(" ORDER BY depth ASC, created_at ASC");

    let mut qb = state.db.query(&query_str)
        .bind(("vid", vault_id))
        .bind(("sid", session_id));
    if let Some(s) = params.status { qb = qb.bind(("status", s)); }
    if let Some(q) = params.q { qb = qb.bind(("q", q)); }

    let mut resp = qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Value> = resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!(rows)))
}

/// Cross-session page search for a vault (no session_id constraint).
async fn search_all_pages(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(params): Query<PagesQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut query_str = "SELECT * FROM import_pages WHERE vault_id = $vid".to_string();
    if params.status.is_some() { query_str.push_str(" AND status = $status"); }
    if params.q.is_some() {
        query_str.push_str(" AND (string::lowercase(title) CONTAINS string::lowercase($q) OR string::lowercase(url) CONTAINS string::lowercase($q) OR (content_md != NONE AND string::lowercase(content_md) CONTAINS string::lowercase($q)))");
    }
    query_str.push_str(" ORDER BY last_crawled DESC LIMIT 20");

    let mut qb = state.db.query(&query_str).bind(("vid", vault_id));
    if let Some(s) = params.status { qb = qb.bind(("status", s)); }
    if let Some(q) = params.q { qb = qb.bind(("q", q)); }

    let mut resp = qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Value> = resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!(rows)))
}

async fn add_import_pages(
    State(state): State<ApiState>,
    Path((vault_id, session_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let pages = body.get("pages")
        .and_then(|v| v.as_array())
        .ok_or((StatusCode::BAD_REQUEST, "Missing pages array".to_string()))?
        .clone();
    let now = Utc::now().timestamp();
    let mut inserted = 0usize;
    for page in &pages {
        let page_id = page.get("page_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let url = page.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let title = page.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
        let parent_url = page.get("parent_url").and_then(|v| v.as_str()).map(|s| s.to_string());
        let depth = page.get("depth").and_then(|v| v.as_i64()).unwrap_or(0);
        let _ = state.db
            .query("INSERT INTO import_pages (vault_id, page_id, session_id, url, title, parent_url, depth, status, created_at) VALUES ($vid, $pid, $sid, $url, $title, $purl, $depth, 'pending', $now) ON DUPLICATE KEY UPDATE title = $title")
            .bind(("vid", vault_id.clone()))
            .bind(("pid", page_id))
            .bind(("sid", session_id.clone()))
            .bind(("url", url))
            .bind(("title", title))
            .bind(("purl", parent_url))
            .bind(("depth", depth))
            .bind(("now", now))
            .await;
        inserted += 1;
    }
    Ok(Json(json!({ "ok": true, "inserted": inserted })))
}

async fn update_import_page(
    State(state): State<ApiState>,
    Path((vault_id, session_id, page_id)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("pending").to_string();
    let note_path = body.get("note_path").and_then(|v| v.as_str()).map(|s| s.to_string());
    let content_md = body.get("content_md").and_then(|v| v.as_str()).map(|s| s.to_string());
    let content_hash = body.get("content_hash").and_then(|v| v.as_str()).map(|s| s.to_string());
    let http_etag = body.get("http_etag").and_then(|v| v.as_str()).map(|s| s.to_string());
    let title = body.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
    // Scope update to vault_id + session_id to prevent cross-vault writes
    state.db
        .query("UPDATE import_pages SET status = $status, note_path = $np, content_md = $cmd, content_hash = $ch, http_etag = $etag, title = $title, last_crawled = $now WHERE page_id = $pid AND vault_id = $vid AND session_id = $sid")
        .bind(("status", status))
        .bind(("np", note_path))
        .bind(("cmd", content_md))
        .bind(("ch", content_hash))
        .bind(("etag", http_etag))
        .bind(("title", title))
        .bind(("now", now))
        .bind(("pid", page_id))
        .bind(("vid", vault_id))
        .bind(("sid", session_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SuggestionsQuery {
    session_id: Option<String>,
    page_id: Option<String>,
}

async fn list_kb_suggestions(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<SuggestionsQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut query_str = "SELECT * FROM kb_suggestions WHERE vault_id = $vid".to_string();
    if q.session_id.is_some() { query_str.push_str(" AND session_id = $sid"); }
    if q.page_id.is_some() { query_str.push_str(" AND page_id = $pid"); }
    query_str.push_str(" ORDER BY created_at DESC");

    let mut qb = state.db.query(&query_str).bind(("vid", vault_id));
    if let Some(sid) = q.session_id { qb = qb.bind(("sid", sid)); }
    if let Some(pid) = q.page_id { qb = qb.bind(("pid", pid)); }

    let mut resp = qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Value> = resp.take(0).unwrap_or_default();
    Ok(Json(json!(rows)))
}

async fn create_kb_suggestion(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let suggestion_id = Uuid::new_v4().to_string();
    let session_id = body.get("session_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    let page_id = body.get("page_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    let title = body.get("title").and_then(|v| v.as_str()).ok_or((StatusCode::BAD_REQUEST, "Missing title".to_string()))?.to_string();
    let template = body.get("template").and_then(|v| v.as_str()).unwrap_or("concept").to_string();
    let content = body.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let reason = body.get("reason").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let now = Utc::now().timestamp();
    state.db
        .query("INSERT INTO kb_suggestions (suggestion_id, vault_id, session_id, page_id, title, template, content, reason, created_at) VALUES ($sgid, $vid, $sid, $pid, $title, $tmpl, $content, $reason, $now)")
        .bind(("sgid", suggestion_id.clone()))
        .bind(("vid", vault_id))
        .bind(("sid", session_id))
        .bind(("pid", page_id))
        .bind(("title", title))
        .bind(("tmpl", template))
        .bind(("content", content))
        .bind(("reason", reason))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "suggestion_id": suggestion_id })))
}

async fn delete_kb_suggestion(
    State(state): State<ApiState>,
    Path((vault_id, suggestion_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state.db
        .query("DELETE FROM kb_suggestions WHERE suggestion_id = $sgid AND vault_id = $vid")
        .bind(("sgid", suggestion_id))
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn get_kb_stats(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    #[derive(serde::Deserialize)]
    struct CountRow { count: i64 }

    // Count notes by frontmatter status (parse from content)
    let mut r = state.db
        .query("SELECT count() AS count FROM notes WHERE vault_id = $vid GROUP ALL")
        .bind(("vid", vault_id.clone())).await.ok();
    let total_notes: i64 = r.as_mut()
        .and_then(|rr| rr.take::<Vec<CountRow>>(0).ok())
        .and_then(|v| v.into_iter().next().map(|row| row.count)).unwrap_or(0);

    let mut r2 = state.db
        .query("SELECT count() AS count FROM knowledge_items WHERE vault_id = $vid GROUP ALL")
        .bind(("vid", vault_id.clone())).await.ok();
    let ki_count: i64 = r2.as_mut()
        .and_then(|rr| rr.take::<Vec<CountRow>>(0).ok())
        .and_then(|v| v.into_iter().next().map(|row| row.count)).unwrap_or(0);

    // 30-day daily trend (notes created)
    use chrono::{Utc, TimeZone};
    let today = Utc::now().date_naive();
    let mut daily_trend = Vec::new();
    for d in (0..30i64).rev() {
        let date = today - chrono::TimeDelta::days(d);
        let date_str = date.format("%Y-%m-%d").to_string();
        let start = Utc.from_utc_datetime(&date.and_hms_opt(0,0,0).unwrap()).timestamp();
        let end   = Utc.from_utc_datetime(&date.and_hms_opt(23,59,59).unwrap()).timestamp();
        let mut dr = state.db
            .query("SELECT count() AS count FROM notes WHERE vault_id = $vid AND modified_at >= $start AND modified_at <= $end GROUP ALL")
            .bind(("vid", vault_id.clone()))
            .bind(("start", start))
            .bind(("end", end))
            .await.ok();
        let count: i64 = dr.as_mut()
            .and_then(|rr| rr.take::<Vec<CountRow>>(0).ok())
            .and_then(|v| v.into_iter().next().map(|row| row.count)).unwrap_or(0);
        daily_trend.push(json!({ "date": date_str, "total": count, "verified": 0 }));
    }

    Ok(Json(json!({
        "total_notes": total_notes,
        "verified": 0,
        "draft": 0,
        "deprecated": 0,
        "no_status": total_notes,
        "knowledge_items": ki_count,
        "topics": [],
        "daily_trend": daily_trend,
    })))
}

async fn list_knowledge_items(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM knowledge_items WHERE vault_id = $vid ORDER BY created_at DESC")
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn create_knowledge_item(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let item_id = Uuid::new_v4().to_string();
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing title".to_string()))?
        .to_string();
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let source_refs = body
        .get("source_refs")
        .map(|v| v.to_string());
    let ai_summary = body
        .get("ai_summary")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO knowledge_items (item_id, vault_id, session_id, title, source_refs, ai_summary, created_at) VALUES ($iid, $vid, $sid, $title, $srefs, $summary, $now)")
        .bind(("iid", item_id.clone()))
        .bind(("vid", vault_id))
        .bind(("sid", session_id))
        .bind(("title", title))
        .bind(("srefs", source_refs))
        .bind(("summary", ai_summary))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "item_id": item_id })))
}

async fn get_knowledge_item(
    State(state): State<ApiState>,
    Path((vault_id, item_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM knowledge_items WHERE item_id = $iid AND vault_id = $vid LIMIT 1")
        .bind(("iid", item_id.clone()))
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let rows: Vec<Value> = resp.take(0).unwrap_or_default();
    match rows.into_iter().next() {
        Some(row) => Ok(Json(row)),
        None => Err((StatusCode::NOT_FOUND, format!("Knowledge item '{}' not found", item_id))),
    }
}

async fn update_knowledge_item(
    State(state): State<ApiState>,
    Path((vault_id, item_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let title = body.get("title").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing title".to_string()))?
        .to_string();
    state
        .db
        .query("UPDATE knowledge_items SET title = $title WHERE item_id = $iid AND vault_id = $vid")
        .bind(("title", title))
        .bind(("iid", item_id))
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete_knowledge_item(
    State(state): State<ApiState>,
    Path((vault_id, item_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Delete item and its chunks, scoped to vault
    state
        .db
        .query("DELETE FROM knowledge_items WHERE item_id = $iid AND vault_id = $vid")
        .bind(("iid", item_id.clone()))
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state
        .db
        .query("DELETE FROM chunks WHERE knowledge_item_id = $iid")
        .bind(("iid", item_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}
