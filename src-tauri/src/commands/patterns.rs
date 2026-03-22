use crate::{error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Serialize, Deserialize)]
pub struct PatternRow {
    pub signature: String,
    pub score: f64,
    pub trigger_count: i64,
    pub speak_count: i64,
    pub deprecated: bool,
    pub semantic_intent: Option<String>,
}

/// Upsert pattern by (vault_id, signature).
/// First occurrence: insert with initial score 0.3.
/// Subsequent: increment trigger_count + refresh last_triggered_at.
#[tauri::command]
pub async fn save_pattern(
    state: State<'_, AppState>,
    vault_id: String,
    signature: String,
) -> Result<(), AppError> {
    state.db.query(
        "INSERT INTO activity_patterns (vault_id, signature, score, trigger_count, speak_count, deprecated)
         VALUES ($vid, $sig, 0.3, 1, 0, false)
         ON DUPLICATE KEY UPDATE
           trigger_count     = trigger_count + 1,
           last_triggered_at = time::now(),
           updated_at        = time::now()"
    )
    .bind(("vid", vault_id))
    .bind(("sig", signature))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// Bayesian score update.
/// spoke=true  → score += 0.2 * (1 - score)  (positive reinforcement)
/// spoke=false → score -= 0.1 * score          (silence penalty)
/// Also clears deprecated flag on positive update.
#[tauri::command]
pub async fn update_pattern_score(
    state: State<'_, AppState>,
    vault_id: String,
    signature: String,
    spoke: bool,
) -> Result<(), AppError> {
    if spoke {
        state.db.query(
            "UPDATE activity_patterns SET
               speak_count = speak_count + 1,
               score       = math::clamp(score + 0.2 * (1.0 - score), 0.0, 1.0),
               deprecated  = false,
               updated_at  = time::now()
             WHERE vault_id = $vid AND signature = $sig"
        )
        .bind(("vid", vault_id))
        .bind(("sig", signature))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    } else {
        // Two-step: decay score, then mark deprecated where score fell below 0.2
        state.db.query(
            "UPDATE activity_patterns SET
               score      = math::clamp(score - 0.1 * score, 0.0, 1.0),
               updated_at = time::now()
             WHERE vault_id = $vid AND signature = $sig;

             UPDATE activity_patterns SET deprecated = true
             WHERE vault_id = $vid AND signature = $sig AND score < 0.2;"
        )
        .bind(("vid", vault_id))
        .bind(("sig", signature))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    }
    Ok(())
}

/// List non-deprecated patterns for a vault, sorted by score desc.
/// min_score filters out low-confidence patterns (default 0.0 = all active).
#[tauri::command]
pub async fn list_patterns(
    state: State<'_, AppState>,
    vault_id: String,
    min_score: Option<f64>,
) -> Result<Vec<PatternRow>, AppError> {
    let min = min_score.unwrap_or(0.0);

    #[derive(Deserialize)]
    struct Row {
        signature: String,
        score: f64,
        trigger_count: i64,
        speak_count: i64,
        deprecated: bool,
        semantic_intent: Option<String>,
    }

    let mut resp = state.db.query(
        "SELECT signature, score, trigger_count, speak_count, deprecated, semantic_intent
         FROM activity_patterns
         WHERE vault_id = $vid AND deprecated = false AND score >= $min
         ORDER BY score DESC"
    )
    .bind(("vid", vault_id))
    .bind(("min", min))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    let rows: Vec<Row> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    Ok(rows.into_iter().map(|r| PatternRow {
        signature:       r.signature,
        score:           r.score,
        trigger_count:   r.trigger_count,
        speak_count:     r.speak_count,
        deprecated:      r.deprecated,
        semantic_intent: r.semantic_intent,
    }).collect())
}

/// Daily decay: multiply all active pattern scores by 0.95.
/// Mark deprecated where decayed score falls below 0.2.
#[tauri::command]
pub async fn decay_patterns(
    state: State<'_, AppState>,
    vault_id: String,
) -> Result<(), AppError> {
    state.db.query(
        "UPDATE activity_patterns SET
           score          = math::clamp(score * 0.95, 0.0, 1.0),
           last_decayed_at = time::now(),
           updated_at     = time::now()
         WHERE vault_id = $vid AND deprecated = false;

         UPDATE activity_patterns SET deprecated = true
         WHERE vault_id = $vid AND score < 0.2 AND deprecated = false;"
    )
    .bind(("vid", vault_id.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// Write back semantic_intent label computed by async embedding (P2-9).
#[tauri::command]
pub async fn set_pattern_intent(
    state: State<'_, AppState>,
    vault_id: String,
    signature: String,
    intent: String,
) -> Result<(), AppError> {
    state.db.query(
        "UPDATE activity_patterns SET
           semantic_intent = $intent,
           updated_at      = time::now()
         WHERE vault_id = $vid AND signature = $sig"
    )
    .bind(("vid", vault_id))
    .bind(("sig", signature))
    .bind(("intent", intent))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}
