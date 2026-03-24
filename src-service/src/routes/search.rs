use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/vaults/:vault_id/search", get(search_notes))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

async fn search_notes(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let query_str = q
        .q
        .filter(|s| !s.is_empty())
        .ok_or((StatusCode::BAD_REQUEST, "Missing query param q".to_string()))?;

    let mut resp = state
        .db
        .query("SELECT path, title, search::score(1) AS score FROM notes WHERE vault_id = $vid AND (title @1@ $q OR content @1@ $q) ORDER BY score DESC LIMIT 20")
        .bind(("vid", vault_id))
        .bind(("q", query_str))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}
