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
            "/vaults/:vault_id/activity-patterns/skill-candidates",
            get(list_skill_candidates),
        )
        .route(
            "/vaults/:vault_id/ratings",
            get(get_conversation_ratings).post(create_rating),
        )
        .route("/vaults/:vault_id/memory/query", get(query_memory))
        .route("/vaults/:vault_id/memory/facts", post(store_facts))
        .route("/vaults/:vault_id/memory/distill", post(distill_facts))
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

// ── Skill Candidates ──────────────────────────────────────────────────────────

async fn list_skill_candidates(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Patterns with score >= 0.7, not deprecated, with semantic_intent set
    let mut resp = state.db
        .query("SELECT signature, score, trigger_count, speak_count, semantic_intent FROM activity_patterns WHERE vault_id = $vid AND deprecated = false AND score >= 0.7 AND semantic_intent != NONE ORDER BY score DESC LIMIT 10")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let patterns: Vec<Value> = resp.take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Exclude patterns that already have a matching skill (by trigger keyword overlap)
    let mut skill_resp = state.db
        .query("SELECT trigger FROM agent_skills WHERE vault_id = $vid AND is_active = true")
        .bind(("vid", vault_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let skills: Vec<Value> = skill_resp.take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let existing_triggers: Vec<String> = skills.iter()
        .filter_map(|s| s["trigger"].as_str().map(|t| t.to_lowercase()))
        .collect();

    let candidates: Vec<Value> = patterns.into_iter().filter(|p| {
        let intent = p["semantic_intent"].as_str().unwrap_or("").to_lowercase();
        if intent.is_empty() { return false; }
        // Skip if any existing skill trigger contains the intent as a keyword
        !existing_triggers.iter().any(|t| t.contains(&intent) || intent.contains(t.as_str()))
    }).collect();

    Ok(Json(json!(candidates)))
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
    category: Option<String>,
    limit: Option<i64>,
}

async fn query_memory(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(params): Query<MemoryQueryParams>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let limit = params.limit.unwrap_or(10).min(50);
    let now_ts = Utc::now().timestamp();

    let keywords: Vec<String> = params.keywords
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect())
        .unwrap_or_default();

    let categories: Vec<String> = params.category
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect())
        .unwrap_or_default();

    let rows: Vec<Value> = if keywords.is_empty() && categories.is_empty() {
        let mut resp = state.db
            .query("SELECT fact_id, content, category, created_at FROM memory_facts WHERE vault_id = $vid AND expires_at > $now ORDER BY created_at DESC LIMIT $limit")
            .bind(("vid", vault_id))
            .bind(("now", now_ts))
            .bind(("limit", limit))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else if !keywords.is_empty() && categories.is_empty() {
        let kw = keywords[0].to_lowercase();
        let mut resp = state.db
            .query("SELECT fact_id, content, category, created_at FROM memory_facts WHERE vault_id = $vid AND expires_at > $now AND string::contains(string::lowercase(content), $kw) ORDER BY created_at DESC LIMIT $limit")
            .bind(("vid", vault_id))
            .bind(("now", now_ts))
            .bind(("kw", kw))
            .bind(("limit", limit))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else if keywords.is_empty() && !categories.is_empty() {
        // category filter only — SurrealDB doesn't support IN with bind arrays cleanly, use OR
        let cat_cond = categories.iter().enumerate()
            .map(|(i, _)| format!("category = $cat{}", i))
            .collect::<Vec<_>>().join(" OR ");
        let mut qb = state.db
            .query(format!("SELECT fact_id, content, category, created_at FROM memory_facts WHERE vault_id = $vid AND expires_at > $now AND ({}) ORDER BY created_at DESC LIMIT $limit", cat_cond))
            .bind(("vid", vault_id))
            .bind(("now", now_ts))
            .bind(("limit", limit));
        for (i, cat) in categories.iter().enumerate() {
            qb = qb.bind((format!("cat{}", i), cat.clone()));
        }
        let mut resp = qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        let kw = keywords[0].to_lowercase();
        let cat_cond = categories.iter().enumerate()
            .map(|(i, _)| format!("category = $cat{}", i))
            .collect::<Vec<_>>().join(" OR ");
        let mut qb = state.db
            .query(format!("SELECT fact_id, content, category, created_at FROM memory_facts WHERE vault_id = $vid AND expires_at > $now AND string::contains(string::lowercase(content), $kw) AND ({}) ORDER BY created_at DESC LIMIT $limit", cat_cond))
            .bind(("vid", vault_id))
            .bind(("now", now_ts))
            .bind(("kw", kw))
            .bind(("limit", limit));
        for (i, cat) in categories.iter().enumerate() {
            qb = qb.bind((format!("cat{}", i), cat.clone()));
        }
        let mut resp = qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    Ok(Json(json!(rows)))
}

// ── Store Facts ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FactInput {
    content: String,
    category: Option<String>,
    conv_id: Option<String>,
}

#[derive(Deserialize)]
struct StoreFactsBody {
    facts: Vec<FactInput>,
}

async fn store_facts(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<StoreFactsBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now_ts = Utc::now().timestamp();
    let mut inserted = 0u32;

    for fact in body.facts {
        let content = fact.content.trim().to_string();
        if content.is_empty() { continue; }

        let category = fact.category.as_deref().unwrap_or("general").to_string();

        // expires_at: personal/rule → 365 days, others → 90 days
        let days = if category == "personal" || category == "rule" { 365i64 } else { 90 };
        let expires_at = now_ts + days * 86400;

        // Dedup: check if a non-expired fact with same prefix (first 40 chars) already exists
        let prefix: String = content.chars().take(40).collect::<String>().to_lowercase();
        let mut check = state.db
            .query("SELECT fact_id FROM memory_facts WHERE vault_id = $vid AND string::startsWith(string::lowercase(content), $prefix) AND expires_at > $now LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .bind(("prefix", prefix))
            .bind(("now", now_ts))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let existing: Vec<Value> = check.take(0)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        if !existing.is_empty() {
            // Refresh expires_at on existing fact
            let fact_id = existing[0]["fact_id"].as_str().unwrap_or("").to_string();
            let _ = state.db
                .query("UPDATE memory_facts SET expires_at = $exp WHERE fact_id = $fid")
                .bind(("exp", expires_at))
                .bind(("fid", fact_id))
                .await;
            continue;
        }

        let fact_id = Uuid::new_v4().to_string();
        state.db
            .query("INSERT INTO memory_facts (fact_id, vault_id, conv_id, content, category, expires_at, created_at) VALUES ($fid, $vid, $cid, $content, $cat, $exp, $now)")
            .bind(("fid", fact_id))
            .bind(("vid", vault_id.clone()))
            .bind(("cid", fact.conv_id.unwrap_or_default()))
            .bind(("content", content))
            .bind(("cat", category))
            .bind(("exp", expires_at))
            .bind(("now", now_ts))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        inserted += 1;
    }

    Ok(Json(json!({ "inserted": inserted })))
}

// ── Distill Facts ──────────────────────────────────────────────────────────────
// Called by Tauri after LLM distillation. Replaces old facts in a category
// with a single high-level summary fact.

#[derive(Deserialize)]
struct DistillFactsBody {
    category: String,
    /// fact_ids to delete (the ones that were distilled)
    source_ids: Vec<String>,
    /// the new summary content produced by LLM
    summary: String,
}

async fn distill_facts(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<DistillFactsBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if body.source_ids.is_empty() || body.summary.trim().is_empty() {
        return Ok(Json(json!({ "ok": false, "reason": "empty input" })));
    }

    let now_ts = Utc::now().timestamp();
    // personal/rule summaries last 365 days, others 180 days (longer than raw facts)
    let days = if body.category == "personal" || body.category == "rule" { 365i64 } else { 180 };
    let expires_at = now_ts + days * 86400;

    // Delete source facts
    for fid in &body.source_ids {
        let _ = state.db
            .query("DELETE memory_facts WHERE fact_id = $fid AND vault_id = $vid")
            .bind(("fid", fid.clone()))
            .bind(("vid", vault_id.clone()))
            .await;
    }

    // Insert summary fact
    let fact_id = uuid::Uuid::new_v4().to_string();
    state.db
        .query("INSERT INTO memory_facts (fact_id, vault_id, conv_id, content, category, expires_at, created_at) VALUES ($fid, $vid, $cid, $content, $cat, $exp, $now)")
        .bind(("fid", fact_id.clone()))
        .bind(("vid", vault_id))
        .bind(("cid", Option::<String>::None))
        .bind(("content", body.summary.trim().to_string()))
        .bind(("cat", body.category))
        .bind(("exp", expires_at))
        .bind(("now", now_ts))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true, "fact_id": fact_id, "replaced": body.source_ids.len() })))
}
