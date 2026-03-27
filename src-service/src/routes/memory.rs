use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api_state::ApiState;
use crate::routes::vault::get_vault_path;

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
        .route(
            "/vaults/:vault_id/activity-patterns/score",
            post(update_pattern_score),
        )
        .route(
            "/vaults/:vault_id/activity-patterns/decay",
            post(decay_patterns),
        )
        .route(
            "/vaults/:vault_id/activity-patterns/intent",
            post(set_pattern_intent).patch(set_pattern_intent),
        )
        .route(
            "/vaults/:vault_id/ratings",
            get(get_conversation_ratings).post(create_rating),
        )
        .route("/vaults/:vault_id/memory/query", get(query_memory))
        .route("/vaults/:vault_id/memory/session", post(save_memory_session))
}

async fn list_memory_rules(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT rule_id, vault_id, pattern_type, pattern, `value`, created_at FROM memory_rules WHERE vault_id = $vid ORDER BY created_at DESC")
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
        .query("INSERT INTO memory_rules (rule_id, vault_id, pattern_type, pattern, `value`, created_at) VALUES ($rid, $vid, $ptype, $pat, $val, $now)")
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
        .query("DELETE FROM memory_rules WHERE vault_id = $vid AND rule_id = $rid")
        .bind(("vid", vault_id))
        .bind(("rid", rule_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize, Default)]
struct PatternListQuery {
    min_score: Option<f64>,
}

async fn list_activity_patterns(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<PatternListQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let min_score = q.min_score.unwrap_or(0.0);
    let mut resp = state
        .db
        .query("SELECT * FROM activity_patterns WHERE vault_id = $vid AND deprecated = false AND score >= $min ORDER BY score DESC")
        .bind(("vid", vault_id))
        .bind(("min", min_score))
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
    let semantic_intent = body
        .get("semantic_intent")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO activity_patterns (vault_id, signature, score, trigger_count, speak_count, deprecated, semantic_intent, created_at, updated_at) VALUES ($vid, $sig, 0.3, 1, 0, false, $intent, $now, $now) ON DUPLICATE KEY UPDATE trigger_count = trigger_count + 1, deprecated = false, updated_at = $now")
        .bind(("vid", vault_id))
        .bind(("sig", signature))
        .bind(("intent", semantic_intent))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct UpdateScoreBody {
    signature: String,
    spoke: bool,
}

async fn update_pattern_score(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<UpdateScoreBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    if body.spoke {
        state
            .db
            .query("UPDATE activity_patterns SET speak_count = speak_count + 1, score = math::clamp(score + 0.2 * (1.0 - score), 0.0, 1.0), deprecated = false, updated_at = $now WHERE vault_id = $vid AND signature = $sig")
            .bind(("vid", vault_id))
            .bind(("sig", body.signature))
            .bind(("now", now))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    } else {
        state
            .db
            .query("UPDATE activity_patterns SET score = math::clamp(score - 0.1 * score, 0.0, 1.0), updated_at = $now WHERE vault_id = $vid AND signature = $sig; UPDATE activity_patterns SET deprecated = true WHERE vault_id = $vid AND signature = $sig AND score < 0.2;")
            .bind(("vid", vault_id))
            .bind(("sig", body.signature))
            .bind(("now", now))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn decay_patterns(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    state
        .db
        .query("UPDATE activity_patterns SET score = math::clamp(score * 0.95, 0.0, 1.0), updated_at = $now WHERE vault_id = $vid; UPDATE activity_patterns SET deprecated = true WHERE vault_id = $vid AND score < 0.1;")
        .bind(("vid", vault_id))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SetIntentBody {
    signature: String,
    semantic_intent: String,
}

async fn set_pattern_intent(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<SetIntentBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Utc::now().timestamp();
    state
        .db
        .query("UPDATE activity_patterns SET semantic_intent = $intent, updated_at = $now WHERE vault_id = $vid AND signature = $sig")
        .bind(("vid", vault_id))
        .bind(("sig", body.signature))
        .bind(("intent", body.semantic_intent))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

// ── Response Ratings ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RatingsQuery {
    conversation_id: Option<String>,
}

async fn get_conversation_ratings(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(params): Query<RatingsQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rows: Vec<Value> = if let Some(conv_id) = params.conversation_id {
        let mut resp = state
            .db
            .query("SELECT * FROM response_ratings WHERE vault_id = $vid AND conversation_id = $cid ORDER BY created_at DESC")
            .bind(("vid", vault_id))
            .bind(("cid", conv_id))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        let mut resp = state
            .db
            .query("SELECT * FROM response_ratings WHERE vault_id = $vid ORDER BY created_at DESC LIMIT 100")
            .bind(("vid", vault_id))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    Ok(Json(json!(rows)))
}

async fn create_rating(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rating_id = Uuid::new_v4().to_string();
    let conversation_id = body.get("conversation_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    let content_hash = body
        .get("content_hash")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing content_hash".to_string()))?
        .to_string();
    let rating = body
        .get("rating")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing rating".to_string()))?
        .to_string();
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO response_ratings (id, rating_id, vault_id, conversation_id, content_hash, rating, created_at) VALUES ($rid, $rid, $vid, $cid, $hash, $rat, $now)")
        .bind(("rid", rating_id.clone()))
        .bind(("vid", vault_id))
        .bind(("cid", conversation_id))
        .bind(("hash", content_hash))
        .bind(("rat", rating))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "id": rating_id })))
}

// ── Memory Query ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct MemoryQueryParams {
    keywords: Option<String>,
    since: Option<String>,
    limit: Option<i64>,
}

async fn query_memory(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(params): Query<MemoryQueryParams>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let limit = params.limit.unwrap_or(10).min(50);
    let since_ts: i64 = params.since
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or(0);

    let keywords: Vec<String> = params.keywords
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect())
        .unwrap_or_default();

    let rows: Vec<Value> = if keywords.is_empty() {
        let mut resp = state
            .db
            .query("SELECT path, title, modified_at AS created_at, string::slice(content, 0, 200) AS snippet FROM notes WHERE vault_id = $vid AND string::contains(path, 'memories/ai_memory_') AND modified_at >= $since ORDER BY modified_at DESC LIMIT $limit")
            .bind(("vid", vault_id))
            .bind(("since", since_ts))
            .bind(("limit", limit))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        let kw = keywords[0].to_lowercase();
        let mut resp = state
            .db
            .query("SELECT path, title, modified_at AS created_at, string::slice(content, 0, 200) AS snippet FROM notes WHERE vault_id = $vid AND string::contains(path, 'memories/ai_memory_') AND string::contains(string::lowercase(content), $kw) AND modified_at >= $since ORDER BY modified_at DESC LIMIT $limit")
            .bind(("vid", vault_id))
            .bind(("kw", kw))
            .bind(("since", since_ts))
            .bind(("limit", limit))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    Ok(Json(json!(rows)))
}

// ── Memory Session ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SaveMemorySessionBody {
    messages: Vec<Value>,
}

async fn save_memory_session(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<SaveMemorySessionBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;

    let now = chrono::Local::now();
    let timestamp = now.format("%Y%m%d_%H%M%S").to_string();
    let display_time = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let title = format!("AI 對話記憶 — {}", display_time);
    let rel_path = format!("memories/ai_memory_{}.md", timestamp);

    // Build markdown content
    let mut content = format!(
        "---\ncreated: {}\nmessage_count: {}\n---\n\n# {}\n\n",
        now.to_rfc3339(),
        body.messages.iter().filter(|m| m["role"].as_str().unwrap_or("") != "tool").count(),
        title
    );
    for msg in &body.messages {
        match msg["role"].as_str().unwrap_or("") {
            "user"      => content.push_str(&format!("**使用者**\n\n{}\n\n---\n\n", msg["content"].as_str().unwrap_or(""))),
            "assistant" => content.push_str(&format!("**助手**\n\n{}\n\n---\n\n", msg["content"].as_str().unwrap_or(""))),
            _ => {}
        }
    }

    // Write to disk
    let abs_path = std::path::Path::new(&vault_path).join(&rel_path);
    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    std::fs::write(&abs_path, &content).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Index in DB
    let word_count = content.split_whitespace().count() as i64;
    let ts = Utc::now().timestamp();
    let _ = state.db
        .query("INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at) VALUES ($vid, $path, $title, $content, $wc, $now, $now) ON DUPLICATE KEY UPDATE title = $title, content = $content, word_count = $wc, modified_at = $now")
        .bind(("vid", vault_id))
        .bind(("path", rel_path.clone()))
        .bind(("title", title))
        .bind(("content", content))
        .bind(("wc", word_count))
        .bind(("now", ts))
        .await;

    Ok(Json(json!({ "ok": true, "path": rel_path })))
}
