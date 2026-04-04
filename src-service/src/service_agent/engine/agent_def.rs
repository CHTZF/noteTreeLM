use serde_json::Value;
use crate::db::SurrealDb;

/// Load agent definition from DB by name + account_id.
pub(crate) async fn load_agent_def(
    db: &SurrealDb,
    name: &str,
    account_id: &str,
) -> Option<Value> {
    let mut resp = match db
        .query("SELECT meta::id(id) as id, def_id, account_id, name, description, kind, tool_names, system_prompt, max_rounds, is_active, is_builtin, trigger, status, skill_ids FROM agent_definitions WHERE name = $name AND account_id = $aid LIMIT 1")
        .bind(("name", name.to_string()))
        .bind(("aid", account_id.to_string()))
        .await
    {
        Ok(r) => r,
        Err(e) => { tracing::error!("[load_agent_def] query error: {}", e); return None; }
    };
    let rows: Vec<Value> = match resp.take(0) {
        Ok(r) => r,
        Err(e) => { tracing::error!("[load_agent_def] take(0) error: {}", e); return None; }
    };
    let result = rows.into_iter().next();
    if result.is_none() {
        tracing::warn!("[load_agent_def] NOT FOUND: name={} account_id={}", name, account_id);
    }
    result
}
