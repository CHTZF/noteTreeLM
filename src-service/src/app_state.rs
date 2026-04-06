use crate::auth::store::AuthStore;
use crate::db::SurrealDb;
use crate::daemon::state::DaemonState;

/// Shared state for all HTTP API route handlers (both :7787 and :7788)
#[derive(Clone)]
pub struct ApiState {
    pub auth: AuthStore,
    pub db: SurrealDb,
    pub daemon: DaemonState,
}

impl ApiState {
    pub fn new(auth: AuthStore, db: SurrealDb, daemon: DaemonState) -> Self {
        Self { auth, db, daemon }
    }

    /// Resolve vault filesystem path from vault_id.
    /// Checks the in-process cache first; falls back to DB query.
    /// Returns empty string if vault_id is empty or vault not found.
    pub async fn resolve_vault_path(&self, vault_id: &str) -> String {
        if vault_id.is_empty() { return String::new(); }
        if let Ok(cache) = self.daemon.vault_path_cache.read() {
            if let Some(path) = cache.get(vault_id) {
                return path.clone();
            }
        }
        #[derive(serde::Deserialize)]
        struct Row { path: String }
        let path: String = self.db
            .query("SELECT path FROM vaults WHERE vault_id = $vid LIMIT 1")
            .bind(("vid", vault_id.to_string()))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| r.path)
            .unwrap_or_default();
        if !path.is_empty() {
            if let Ok(mut cache) = self.daemon.vault_path_cache.write() {
                cache.insert(vault_id.to_string(), path.clone());
            }
        }
        path
    }
}
