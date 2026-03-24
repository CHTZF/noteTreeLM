use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/vaults/:vault_id/kb/sessions",
            get(list_import_sessions).post(create_import_session),
        )
        .route("/vaults/:vault_id/kb/sessions/:session_id", get(get_import_session))
        .route(
            "/vaults/:vault_id/kb/sessions/:session_id/pages",
            get(list_import_pages),
        )
        .route(
            "/vaults/:vault_id/kb/items",
            get(list_knowledge_items).post(create_knowledge_item),
        )
}

async fn list_import_sessions(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM import_sessions WHERE vault_id = $vid ORDER BY created_at DESC")
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
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

async fn list_import_pages(
    State(state): State<ApiState>,
    Path((vault_id, session_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM import_pages WHERE vault_id = $vid AND session_id = $sid ORDER BY depth ASC, created_at ASC")
        .bind(("vid", vault_id))
        .bind(("sid", session_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
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
