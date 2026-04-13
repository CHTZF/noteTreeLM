pub mod embed;
pub mod tracker;

pub use embed::EmbedModel;
pub use tracker::SpeakerTracker;

use std::sync::Arc;
use crate::db::SurrealDb;

/// Read `diarize_model_path` from the settings DB and try to load the model.
/// Returns `Ok(Some(model))` on success, `Ok(None)` if the path is unset/empty,
/// and `Err(msg)` if the path is set but the model fails to load.
pub async fn load_from_db(db: &SurrealDb) -> Result<Option<Arc<EmbedModel>>, String> {
    #[derive(serde::Deserialize)]
    struct Row { value: String }

    let path: Option<String> = db
        .query("SELECT `value` FROM `settings` WHERE `key` = 'diarize_model_path' LIMIT 1")
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
        .and_then(|rows| rows.into_iter().next())
        .map(|r| r.value)
        .filter(|v| !v.trim().is_empty());

    match path {
        None => Ok(None),
        Some(p) => {
            let model = EmbedModel::load(&p)?;
            Ok(Some(Arc::new(model)))
        }
    }
}
