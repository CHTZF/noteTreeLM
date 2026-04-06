use serde_json::{json, Value};
use crate::db::SurrealDb;

/// Vault-scoped, embedding-indexed memory facts stored in `memory_facts` DB table.
///
/// # Memory semantics
/// - **read** — [`recall`]: FTS + cosine-similarity re-rank to retrieve the most relevant
///   facts for the current conversation turn.
/// - **write** — [`store_facts`]: embed each fact string, deduplicate against existing
///   entries, and upsert into `memory_facts`.
/// - **evict** — [`prune_before`]: delete `memory_facts` rows whose `expires_at` timestamp
///   has passed.  Intended to be called periodically (e.g. on agent startup) to bound
///   vault growth.
///
/// `SemanticMemory` is intentionally stateless beyond its I/O dependencies — the facts
/// themselves live in the DB and are never cached in process memory.
#[allow(dead_code)]
pub(crate) struct SemanticMemory {
    db:            SurrealDb,
    client:        reqwest::Client,
    embedding_url: Option<String>,
}

#[allow(dead_code)]
impl SemanticMemory {
    pub(crate) fn new(
        db:            SurrealDb,
        client:        reqwest::Client,
        embedding_url: Option<String>,
    ) -> Self {
        Self { db, client, embedding_url }
    }

    /// Recall up to `limit` memory facts relevant to `keywords`.
    pub(crate) async fn recall(
        &self,
        vault_id:   &str,
        account_id: &str,
        keywords:   &[String],
        limit:      u64,
    ) -> Vec<Value> {
        vault_query_memory_with_limit(
            &self.client,
            &self.embedding_url,
            &self.db,
            vault_id,
            account_id,
            keywords,
            limit,
        ).await
    }

    /// Embed and persist `facts` into the `memory_facts` table.
    pub(crate) async fn store_facts(
        &self,
        vault_id:   &str,
        account_id: &str,
        conv_id:    &str,
        facts:      Vec<Value>,
    ) -> Result<Value, String> {
        crate::service::harness::tools::memory_tools::save_memory_facts(
            &self.client,
            &self.db,
            vault_id,
            account_id,
            conv_id,
            facts,
            &self.embedding_url,
        ).await
    }

    /// Evict all `memory_facts` rows whose `expires_at` is ≤ `before_ts` (Unix seconds).
    /// Returns the number of rows deleted.
    pub(crate) async fn prune_before(
        &self,
        vault_id:  &str,
        before_ts: i64,
    ) -> usize {
        #[derive(serde::Deserialize)]
        struct Row { count: u64 }

        let count: u64 = self.db
            .query(
                "SELECT count() AS count FROM memory_facts \
                 WHERE vault_id = $vid AND expires_at <= $ts GROUP ALL",
            )
            .bind(("vid", vault_id.to_string()))
            .bind(("ts", before_ts))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| r.count)
            .unwrap_or(0);

        let _ = self.db
            .query("DELETE memory_facts WHERE vault_id = $vid AND expires_at <= $ts")
            .bind(("vid", vault_id.to_string()))
            .bind(("ts", before_ts))
            .await;

        count as usize
    }

    /// Return the count of unexpired memory facts for the given vault + account.
    pub(crate) async fn fact_count(&self, vault_id: &str, account_id: &str) -> usize {
        #[derive(serde::Deserialize)]
        struct Row { count: u64 }
        let now = chrono::Utc::now().timestamp();
        self.db
            .query(
                "SELECT count() AS count FROM memory_facts \
                 WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now GROUP ALL",
            )
            .bind(("vid", vault_id.to_string()))
            .bind(("aid", account_id.to_string()))
            .bind(("now", now))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| r.count as usize)
            .unwrap_or(0)
    }
}

// ── vault_query_memory_with_limit ─────────────────────────────────────────────
//
// Semantic search over `memory_facts` with cosine-similarity re-ranking.
// Owned here because it operates exclusively on the memory layer.
// Previously lived in engine/context.rs alongside unrelated concerns.

/// Query memory facts using semantic search (embedding cosine similarity).
/// Falls back to keyword regex if embedding server is unavailable or query embed fails.
/// Never errors; returns empty vec on failure.
pub(crate) async fn vault_query_memory_with_limit(
    client:        &reqwest::Client,
    embedding_url: &Option<String>,
    db:            &SurrealDb,
    vault_id:      &str,
    account_id:    &str,
    keywords:      &[String],
    limit:         u64,
) -> Vec<Value> {
    let now = chrono::Utc::now().timestamp();
    #[derive(serde::Deserialize)]
    struct Row { fact_id: Option<String>, content: String, category: String, embedding: Option<Vec<f32>> }

    // Try semantic search first when we have keywords and an embedding server.
    if !keywords.is_empty() {
        let query_text = keywords.join(" ");
        if let Some(query_vec) = crate::embedding::embedder::embed_text(client, embedding_url, &query_text).await {
            if !query_vec.is_empty() {
                let rows: Vec<Row> = db
                    .query("SELECT fact_id, content, category, embedding FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now AND embedding IS NOT NONE")
                    .bind(("vid", vault_id.to_string()))
                    .bind(("aid", account_id.to_string()))
                    .bind(("now", now))
                    .await
                    .ok()
                    .and_then(|mut r| r.take(0).ok())
                    .unwrap_or_default();

                if !rows.is_empty() {
                    let mut scored: Vec<(f32, Row)> = rows.into_iter().filter_map(|row| {
                        let emb = row.embedding.as_ref()?;
                        if emb.is_empty() { return None; }
                        let score = crate::embedding::embedder::cosine_sim(&query_vec, emb);
                        Some((score, row))
                    }).collect();
                    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                    return scored.into_iter()
                        .take(limit as usize)
                        .map(|(_, r)| json!({
                            "fact_id": r.fact_id.unwrap_or_default(),
                            "content": r.content,
                            "category": r.category,
                        }))
                        .collect();
                }
            }
        }
    }

    // Fallback: no embedding server, no keywords, or no facts with embeddings — use recency / keyword regex.
    let rows: Vec<Row> = if keywords.is_empty() {
        db.query("SELECT fact_id, content, category FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND expires_at > $now ORDER BY created_at DESC LIMIT $lim")
            .bind(("vid", vault_id.to_string()))
            .bind(("aid", account_id.to_string()))
            .bind(("now", now))
            .bind(("lim", limit))
            .await
            .ok()
            .and_then(|mut r| r.take(0).ok())
            .unwrap_or_default()
    } else {
        let mut collected: Vec<Row> = Vec::new();
        for kw in keywords.iter().take(3) {
            let mut rows: Vec<Row> = db
                .query("SELECT fact_id, content, category FROM memory_facts WHERE vault_id = $vid AND account_id = $aid AND content ~ $kw AND expires_at > $now LIMIT $lim")
                .bind(("vid", vault_id.to_string()))
                .bind(("aid", account_id.to_string()))
                .bind(("kw", kw.clone()))
                .bind(("now", now))
                .bind(("lim", limit))
                .await
                .ok()
                .and_then(|mut r| r.take(0).ok())
                .unwrap_or_default();
            collected.append(&mut rows);
        }
        collected
    };
    rows.into_iter().map(|r| json!({
        "fact_id": r.fact_id.unwrap_or_default(),
        "content": r.content,
        "category": r.category,
    })).collect()
}
