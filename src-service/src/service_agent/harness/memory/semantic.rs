use serde_json::Value;
use crate::db::SurrealDb;

/// Vault-scoped, embedding-indexed memory facts stored in `memory_facts` DB table.
///
/// # Memory semantics
/// - **read** — [`recall`]: FTS + cosine-similarity re-rank to retrieve the most relevant
///   facts for the current conversation turn.  Delegates to the existing
///   `vault_query_memory_with_limit` implementation in `engine::context`.
/// - **write** — [`store_facts`]: embed each fact string, deduplicate against existing
///   entries, and upsert into `memory_facts`.  Delegates to `memory_tools::save_memory_facts`.
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
    ///
    /// Internally performs embedding-based cosine similarity scoring when an embedding
    /// server is available, falling back to keyword regex search otherwise.
    /// Never errors — returns an empty `Vec` on failure.
    pub(crate) async fn recall(
        &self,
        vault_id:   &str,
        account_id: &str,
        keywords:   &[String],
        limit:      u64,
    ) -> Vec<Value> {
        crate::service_agent::engine::context::vault_query_memory_with_limit(
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
    ///
    /// Each element in `facts` is either a plain string `"fact text"` or an object
    /// `{"content": "...", "category": "personal|preference|project|rule|general"}`.
    /// Duplicate detection is performed by cosine similarity — near-identical facts
    /// are skipped rather than inserted.
    pub(crate) async fn store_facts(
        &self,
        vault_id:   &str,
        account_id: &str,
        conv_id:    &str,
        facts:      Vec<Value>,
    ) -> Result<Value, String> {
        crate::service_agent::tools::memory_tools::save_memory_facts(
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
    ///
    /// This is a soft-delete: the DB rows are removed; vault filesystem files are not
    /// affected (memory facts live in the DB, not in vault `.md` files).
    /// Returns the number of rows deleted.
    pub(crate) async fn prune_before(
        &self,
        vault_id:  &str,
        before_ts: i64,
    ) -> usize {
        #[derive(serde::Deserialize)]
        struct Row { count: u64 }

        // Count first so we can return the deleted count.
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
            .query(
                "DELETE memory_facts WHERE vault_id = $vid AND expires_at <= $ts",
            )
            .bind(("vid", vault_id.to_string()))
            .bind(("ts", before_ts))
            .await;

        count as usize
    }

    /// Return the count of unexpired memory facts for the given vault + account.
    /// Useful for UI display and for deciding whether `recall` is likely to find anything.
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
