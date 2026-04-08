use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::app_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
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
        .route("/vaults/:vault_id/memory/graph", get(memory_graph))
        .route("/vaults/:vault_id/memory/facts", post(store_facts))
        .route("/vaults/:vault_id/memory/facts/:fact_id", delete(delete_fact))
        .route("/vaults/:vault_id/memory/distill", post(distill_facts))
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
        .query("SELECT *, record::id(id) AS id FROM activity_patterns WHERE vault_id = $vid AND deprecated = false AND score >= $min ORDER BY score DESC")
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
            .query("SELECT *, record::id(id) AS id FROM response_ratings WHERE vault_id = $vid AND conversation_id = $cid ORDER BY created_at DESC")
            .bind(("vid", vault_id))
            .bind(("cid", conv_id))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        let mut resp = state
            .db
            .query("SELECT *, record::id(id) AS id FROM response_ratings WHERE vault_id = $vid ORDER BY created_at DESC LIMIT 100")
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
    /// Keyword filter (comma-separated)
    keywords: Option<String>,
    /// Category filter (comma-separated)
    category: Option<String>,
    /// Max results
    limit: Option<i64>,
    /// Semantic text query — when provided, computes embedding and ranks by cosine similarity.
    /// Takes priority over keyword search.
    q: Option<String>,
}

async fn query_memory(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
    Query(params): Query<MemoryQueryParams>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let limit = params.limit.unwrap_or(10).min(50);
    let now_ts = Utc::now().timestamp();

    // Resolve account_id from Bearer token (empty = anonymous, no account filter)
    let account_id = crate::routes::auth::account_id_from_token(&state.db, &headers).await;
    let has_account = !account_id.is_empty();

    let rows: Vec<Value> = if let Some(q_text) = params.q.as_deref().filter(|s| !s.is_empty()) {
        // ── Semantic search: compute query embedding, rank by cosine similarity ──
        let embedding_url = Some(state.daemon.embedding_url.clone());
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        let query_vec = crate::embedding::embedder::embed_text(&http_client, &embedding_url, q_text).await;

        if let Some(qvec) = query_vec {
            // Use SurrealDB vector::similarity::cosine for ranking.
            // Only facts with array embeddings participate; NULL embeddings get score NULL
            // and will be excluded by the ORDER BY (SurrealDB sorts NULLs last in DESC).
            let sql = if has_account {
                "SELECT fact_id, content, category, created_at, \
                 vector::similarity::cosine(embedding, $qvec) AS score \
                 FROM memory_facts \
                 WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now \
                 AND embedding IS NOT NONE \
                 ORDER BY score DESC LIMIT $limit"
            } else {
                "SELECT fact_id, content, category, created_at, \
                 vector::similarity::cosine(embedding, $qvec) AS score \
                 FROM memory_facts \
                 WHERE vault_id = $vid AND expires_at > $now \
                 AND embedding IS NOT NONE \
                 ORDER BY score DESC LIMIT $limit"
            };
            let mut qb = state.db.query(sql)
                .bind(("vid", vault_id.clone()))
                .bind(("now", now_ts))
                .bind(("limit", limit))
                .bind(("qvec", qvec));
            if has_account { qb = qb.bind(("aid", account_id.clone())); }
            let mut resp = qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        } else {
            // Embedder unavailable — fall back to keyword search using q_text
            let kw = q_text.to_lowercase();
            let sql = if has_account {
                "SELECT fact_id, content, category, created_at FROM memory_facts \
                 WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now \
                 AND string::contains(string::lowercase(content), $kw) \
                 ORDER BY created_at DESC LIMIT $limit"
            } else {
                "SELECT fact_id, content, category, created_at FROM memory_facts \
                 WHERE vault_id = $vid AND expires_at > $now \
                 AND string::contains(string::lowercase(content), $kw) \
                 ORDER BY created_at DESC LIMIT $limit"
            };
            let mut qb = state.db.query(sql)
                .bind(("vid", vault_id.clone()))
                .bind(("now", now_ts))
                .bind(("kw", kw))
                .bind(("limit", limit));
            if has_account { qb = qb.bind(("aid", account_id.clone())); }
            let mut resp = qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        }
    } else {
        // ── Keyword / category filter search ──
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

        let aid_clause = if has_account { " AND account_id = $aid" } else { "" };

        let mut qb = if keywords.is_empty() && categories.is_empty() {
            state.db.query(format!(
                "SELECT fact_id, content, category, created_at FROM memory_facts \
                 WHERE vault_id = $vid{} AND expires_at > $now \
                 ORDER BY created_at DESC LIMIT $limit", aid_clause))
                .bind(("vid", vault_id.clone())).bind(("now", now_ts)).bind(("limit", limit))
        } else if !keywords.is_empty() && categories.is_empty() {
            state.db.query(format!(
                "SELECT fact_id, content, category, created_at FROM memory_facts \
                 WHERE vault_id = $vid{} AND expires_at > $now \
                 AND string::contains(string::lowercase(content), $kw) \
                 ORDER BY created_at DESC LIMIT $limit", aid_clause))
                .bind(("vid", vault_id.clone())).bind(("now", now_ts))
                .bind(("kw", keywords[0].to_lowercase())).bind(("limit", limit))
        } else if keywords.is_empty() {
            let cat_cond = categories.iter().enumerate()
                .map(|(i, _)| format!("category = $cat{}", i)).collect::<Vec<_>>().join(" OR ");
            let mut q = state.db.query(format!(
                "SELECT fact_id, content, category, created_at FROM memory_facts \
                 WHERE vault_id = $vid{} AND expires_at > $now AND ({}) \
                 ORDER BY created_at DESC LIMIT $limit", aid_clause, cat_cond))
                .bind(("vid", vault_id.clone())).bind(("now", now_ts)).bind(("limit", limit));
            for (i, cat) in categories.iter().enumerate() { q = q.bind((format!("cat{}", i), cat.clone())); }
            q
        } else {
            let cat_cond = categories.iter().enumerate()
                .map(|(i, _)| format!("category = $cat{}", i)).collect::<Vec<_>>().join(" OR ");
            let mut q = state.db.query(format!(
                "SELECT fact_id, content, category, created_at FROM memory_facts \
                 WHERE vault_id = $vid{} AND expires_at > $now \
                 AND string::contains(string::lowercase(content), $kw) AND ({}) \
                 ORDER BY created_at DESC LIMIT $limit", aid_clause, cat_cond))
                .bind(("vid", vault_id.clone())).bind(("now", now_ts))
                .bind(("kw", keywords[0].to_lowercase())).bind(("limit", limit));
            for (i, cat) in categories.iter().enumerate() { q = q.bind((format!("cat{}", i), cat.clone())); }
            q
        };
        if has_account { qb = qb.bind(("aid", account_id.clone())); }
        let mut resp = qb.await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        resp.take(0).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    // Increment inject_count for each returned fact — spawn so it doesn't block response
    for row in &rows {
        if let Some(fid) = row["fact_id"].as_str() {
            let db = state.db.clone();
            let fid = fid.to_string();
            tokio::spawn(async move {
                let _ = db
                    .query("UPDATE memory_facts SET inject_count = (inject_count ?? 0) + 1 WHERE fact_id = $fid")
                    .bind(("fid", fid))
                    .await;
            });
        }
    }

    Ok(Json(json!(rows)))
}

// ── Store Facts ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FactInput {
    content: String,
    category: Option<String>,
    conv_id: Option<String>,
    /// Caller-supplied account_id (optional; falls back to Bearer token lookup)
    account_id: Option<String>,
    /// Pre-computed embedding as float array (optional; server computes if absent)
    embedding: Option<Vec<f32>>,
}

#[derive(Deserialize)]
struct StoreFactsBody {
    facts: Vec<FactInput>,
    note_refs: Option<Vec<String>>,
}

async fn store_facts(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<StoreFactsBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Resolve account_id from Bearer token (best-effort; empty string if anonymous)
    let bearer_account_id: String = {
        use crate::routes::auth::extract_bearer;
        #[derive(serde::Deserialize)]
        struct Row { username: String }
        if let Some(token) = extract_bearer(&headers) {
            let now = Utc::now().timestamp();
            state.db
                .query("SELECT username FROM sessions WHERE token = $t AND expires_at > $now LIMIT 1")
                .bind(("t", token))
                .bind(("now", now))
                .await
                .ok()
                .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
                .and_then(|rows| rows.into_iter().next())
                .map(|r| r.username)
                .unwrap_or_default()
        } else {
            String::new()
        }
    };

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let embedding_url = Some(state.daemon.embedding_url.clone());
    let now_ts = Utc::now().timestamp();
    let mut inserted = 0u32;
    let note_refs = body.note_refs.unwrap_or_default();
    let mut new_fact_ids: Vec<String> = Vec::new();

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

        // Compute embedding server-side if caller didn't supply one (stored as native array)
        let embedding_vec: Option<Vec<f32>> = if fact.embedding.is_some() {
            fact.embedding
        } else {
            crate::embedding::embedder::embed_text(&http_client, &embedding_url, &content).await
        };

        // Resolve account_id: prefer per-fact field, fall back to Bearer token, then empty
        let fact_account_id = fact.account_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| bearer_account_id.clone());

        let fact_id = Uuid::new_v4().to_string();
        state.db
            .query("INSERT INTO memory_facts (fact_id, vault_id, account_id, conv_id, content, category, expires_at, created_at, embedding) VALUES ($fid, $vid, $aid, $cid, $content, $cat, $exp, $now, $emb)")
            .bind(("fid", fact_id.clone()))
            .bind(("vid", vault_id.clone()))
            .bind(("aid", fact_account_id))
            .bind(("cid", fact.conv_id.unwrap_or_default()))
            .bind(("content", content.clone()))
            .bind(("cat", category.clone()))
            .bind(("exp", expires_at))
            .bind(("now", now_ts))
            .bind(("emb", embedding_vec))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        // Upsert graph node for this memory fact
        let node_id = format!("memory:{}:{}", vault_id, fact_id);
        let label = content.chars().take(60).collect::<String>();
        let _ = state.db
            .query(
                "INSERT INTO graph_nodes (vault_id, node_id, node_type, label, created_at) \
                 VALUES ($vid, $nid, 'memory_fact', $label, $now) \
                 ON DUPLICATE KEY UPDATE label = $label",
            )
            .bind(("vid", vault_id.clone()))
            .bind(("nid", node_id.clone()))
            .bind(("label", label))
            .bind(("now", now_ts))
            .await;

        new_fact_ids.push(node_id);
        inserted += 1;
    }

    // Create graph edges: each new memory_fact → each referenced note
    for fact_node_id in &new_fact_ids {
        for note_ref in &note_refs {
            let note_node_id = format!("{}:{}", vault_id, note_ref);
            let edge_id = Uuid::new_v4().to_string();
            let _ = state.db
                .query(
                    "INSERT INTO graph_edges (edge_id, vault_id, source_id, target_id, edge_type, weight) \
                     VALUES ($eid, $vid, $src, $tgt, 'fact_source', 1.0)",
                )
                .bind(("eid", edge_id))
                .bind(("vid", vault_id.clone()))
                .bind(("src", fact_node_id.clone()))
                .bind(("tgt", note_node_id))
                .await;
        }
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
    headers: HeaderMap,
    Json(body): Json<DistillFactsBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if body.source_ids.is_empty() || body.summary.trim().is_empty() {
        return Ok(Json(json!({ "ok": false, "reason": "empty input" })));
    }

    let account_id = crate::routes::auth::account_id_from_token(&state.db, &headers).await;
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

    // Compute embedding for the summary so it's discoverable by semantic search
    let embedding_url = Some(state.daemon.embedding_url.clone());
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let summary = body.summary.trim().to_string();
    let embedding: Option<Vec<f32>> =
        crate::embedding::embedder::embed_text(&http_client, &embedding_url, &summary).await;

    // Insert summary fact with embedding and account_id
    let fact_id = uuid::Uuid::new_v4().to_string();
    state.db
        .query("INSERT INTO memory_facts (fact_id, vault_id, account_id, conv_id, content, category, expires_at, created_at, embedding) VALUES ($fid, $vid, $aid, $cid, $content, $cat, $exp, $now, $emb)")
        .bind(("fid", fact_id.clone()))
        .bind(("vid", vault_id))
        .bind(("aid", account_id))
        .bind(("cid", Option::<String>::None))
        .bind(("content", summary))
        .bind(("cat", body.category))
        .bind(("exp", expires_at))
        .bind(("now", now_ts))
        .bind(("emb", embedding))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true, "fact_id": fact_id, "replaced": body.source_ids.len() })))
}

// ── Memory Graph ───────────────────────────────────────────────────────────────

async fn memory_graph(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now_ts = Utc::now().timestamp();
    let account_id = crate::routes::auth::account_id_from_token(&state.db, &headers).await;
    let has_account = !account_id.is_empty();

    // All non-expired memory facts (account-scoped)
    #[derive(serde::Deserialize)]
    struct FactRow {
        fact_id: String,
        content: String,
        category: String,
        expires_at: i64,
        inject_count: Option<i64>,
        #[allow(dead_code)]
        created_at: Option<i64>,
    }
    let facts_sql = if has_account {
        "SELECT fact_id, content, category, expires_at, inject_count, created_at FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now ORDER BY created_at DESC"
    } else {
        "SELECT fact_id, content, category, expires_at, inject_count, created_at FROM memory_facts WHERE vault_id = $vid AND expires_at > $now ORDER BY created_at DESC"
    };
    let mut resp = state.db
        .query(facts_sql)
        .bind(("vid", vault_id.clone()))
        .bind(("aid", account_id))
        .bind(("now", now_ts))
        .await
        .map_err(|e| { tracing::error!("memory_graph facts query failed vault={}: {}", vault_id, e); (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()) })?;
    let facts: Vec<FactRow> = resp.take(0).map_err(|e| { tracing::error!("memory_graph facts take failed vault={}: {}", vault_id, e); (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()) })?;

    // All fact_source edges for this vault
    #[derive(serde::Deserialize)]
    struct EdgeRow { source_id: String, target_id: String }
    let mut eresp = state.db
        .query("SELECT source_id, target_id FROM graph_edges WHERE vault_id = $vid AND edge_type = 'fact_source'")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| { tracing::error!("memory_graph edges query failed vault={}: {}", vault_id, e); (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()) })?;
    let edges: Vec<EdgeRow> = eresp.take(0).map_err(|e| { tracing::error!("memory_graph edges take failed vault={}: {}", vault_id, e); (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()) })?;

    // Collect referenced note node_ids
    let note_ids: std::collections::HashSet<String> = edges.iter()
        .map(|e| e.target_id.clone())
        .collect();

    // Fetch note info for referenced nodes
    #[derive(serde::Deserialize)]
    struct NoteRow { path: String, title: String }
    let mut nresp = state.db
        .query("SELECT path, title FROM notes WHERE vault_id = $vid")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| { tracing::error!("memory_graph notes query failed vault={}: {}", vault_id, e); (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()) })?;
    let note_rows: Vec<NoteRow> = nresp.take(0).map_err(|e| { tracing::error!("memory_graph notes take failed vault={}: {}", vault_id, e); (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()) })?;

    let note_prefix = format!("{}:", vault_id);
    let note_nodes: Vec<Value> = note_rows.iter()
        .filter(|n| note_ids.contains(&format!("{}{}", note_prefix, n.path)))
        .map(|n| json!({
            "node_id": format!("{}{}", note_prefix, n.path),
            "node_type": "note",
            "label": n.title,
            "file_path": n.path,
        }))
        .collect();

    let fact_nodes: Vec<Value> = facts.iter().map(|f| {
        let node_id = format!("memory:{}:{}", vault_id, f.fact_id);
        json!({
            "node_id": node_id,
            "node_type": "memory_fact",
            "fact_id": f.fact_id,
            "label": f.content.chars().take(60).collect::<String>(),
            "content": f.content,
            "category": f.category,
            "expires_at": f.expires_at,
            "inject_count": f.inject_count.unwrap_or(0),
        })
    }).collect();

    let edge_list: Vec<Value> = edges.iter().map(|e| json!({
        "source_id": e.source_id,
        "target_id": e.target_id,
        "edge_type": "fact_source",
    })).collect();

    let mut all_nodes = fact_nodes;
    all_nodes.extend(note_nodes);
    Ok(Json(json!({
        "nodes": all_nodes,
        "edges": edge_list,
    })))
}

// ── Delete Fact ────────────────────────────────────────────────────────────────

async fn delete_fact(
    State(state): State<ApiState>,
    Path((vault_id, fact_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let node_id = format!("memory:{}:{}", vault_id, fact_id);

    let _ = state.db
        .query("DELETE memory_facts WHERE fact_id = $fid AND vault_id = $vid")
        .bind(("fid", fact_id))
        .bind(("vid", vault_id.clone()))
        .await;

    let _ = state.db
        .query("DELETE FROM graph_nodes WHERE node_id = $nid AND vault_id = $vid")
        .bind(("nid", node_id.clone()))
        .bind(("vid", vault_id.clone()))
        .await;

    let _ = state.db
        .query("DELETE FROM graph_edges WHERE source_id = $nid AND vault_id = $vid")
        .bind(("nid", node_id))
        .bind(("vid", vault_id))
        .await;

    Ok(Json(json!({ "ok": true })))
}
