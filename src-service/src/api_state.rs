use crate::auth::store::AuthStore;
use crate::db::SurrealDb;
use crate::state::DaemonState;

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
}
