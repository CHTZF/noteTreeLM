use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route(
            "/vaults/:vault_id/memory/rules",
            get(list_memory_rules).post(create_memory_rule),
        )
        .route(
            "/vaults/:vault_id/memory/rules/:rule_id",
            delete(delete_memory_rule),
        )
        .route(
            "/vaults/:vault_id/activity-patterns",
            get(list_activity_patterns).post(upsert_activity_pattern),
        )
}

async fn list_memory_rules(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM memory_rules WHERE vault_id = $vid ORDER BY created_at DESC")
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn create_memory_rule(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rule_id = Uuid::new_v4().to_string();
    let pattern_type = body
        .get("pattern_type")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing pattern_type".to_string()))?
        .to_string();
    let pattern = body
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing pattern".to_string()))?
        .to_string();
    let value = body
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO memory_rules (id, vault_id, pattern_type, pattern, value, created_at) VALUES ($rid, $vid, $ptype, $pat, $val, $now)")
        .bind(("rid", rule_id.clone()))
        .bind(("vid", vault_id))
        .bind(("ptype", pattern_type))
        .bind(("pat", pattern))
        .bind(("val", value))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "id": rule_id })))
}

async fn delete_memory_rule(
    State(state): State<ApiState>,
    Path((vault_id, rule_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state
        .db
        .query("DELETE FROM memory_rules WHERE vault_id = $vid AND id = $rid")
        .bind(("vid", vault_id))
        .bind(("rid", rule_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

async fn list_activity_patterns(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM activity_patterns WHERE vault_id = $vid AND deprecated = false ORDER BY score DESC")
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn upsert_activity_pattern(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let signature = body
        .get("signature")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing signature".to_string()))?
        .to_string();
    let score = body.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let semantic_intent = body
        .get("semantic_intent")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO activity_patterns (vault_id, signature, score, trigger_count, speak_count, deprecated, semantic_intent, created_at, updated_at) VALUES ($vid, $sig, $score, 0, 0, false, $intent, $now, $now) ON DUPLICATE KEY UPDATE score = $score, semantic_intent = $intent, updated_at = $now")
        .bind(("vid", vault_id))
        .bind(("sig", signature))
        .bind(("score", score))
        .bind(("intent", semantic_intent))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}
