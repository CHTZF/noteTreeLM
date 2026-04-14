use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/meetings", get(list_meetings))
        .route("/meetings/:id", get(get_meeting).delete(delete_meeting))
        .route("/meetings/:id/rename-speaker", post(rename_speaker))
}

#[derive(Deserialize)]
struct ListQuery {
    vault_id: Option<String>,
    limit: Option<u64>,
    offset: Option<u64>,
}

async fn list_meetings(
    State(state): State<ApiState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(50).min(200);
    let offset = q.offset.unwrap_or(0);

    #[derive(serde::Deserialize, serde::Serialize)]
    struct Row {
        meeting_id: String,
        vault_id: Option<String>,
        account_id: Option<String>,
        language: Option<String>,
        started_at: Option<i64>,
        ended_at: Option<i64>,
        status: Option<String>,
        note_path: Option<String>,
        wav_path: Option<String>,
        speaker_names_json: Option<String>,
    }

    let mut r = if let Some(vid) = &q.vault_id {
        state.db
            .query("SELECT meeting_id, vault_id, account_id, language, started_at, ended_at, status, note_path, wav_path, speaker_names_json FROM meetings WHERE vault_id = $vid ORDER BY started_at DESC LIMIT $lim START $off")
            .bind(("vid", vid.clone()))
            .bind(("lim", limit))
            .bind(("off", offset))
            .await
    } else {
        state.db
            .query("SELECT meeting_id, vault_id, account_id, language, started_at, ended_at, status, note_path, wav_path, speaker_names_json FROM meetings ORDER BY started_at DESC LIMIT $lim START $off")
            .bind(("lim", limit))
            .bind(("off", offset))
            .await
    }.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Row> = r.take(0).unwrap_or_default();
    Ok(Json(json!({ "meetings": rows })))
}

async fn get_meeting(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    #[derive(serde::Deserialize, serde::Serialize)]
    struct MeetingRow {
        meeting_id: String,
        vault_id: Option<String>,
        account_id: Option<String>,
        language: Option<String>,
        started_at: Option<i64>,
        ended_at: Option<i64>,
        status: Option<String>,
        note_path: Option<String>,
        wav_path: Option<String>,
        speaker_names_json: Option<String>,
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    struct SegRow {
        seg_index: i64,
        text: String,
        ts_ms: i64,
        chunk_start_ms: Option<i64>,
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    struct SpanRow {
        speaker_id: String,
        start_ms: i64,
        end_ms: i64,
    }

    let mut mr = state.db
        .query("SELECT meeting_id, vault_id, account_id, language, started_at, ended_at, status, note_path, wav_path, speaker_names_json FROM meetings WHERE meeting_id = $mid LIMIT 1")
        .bind(("mid", id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let meeting = mr.take::<Vec<MeetingRow>>(0)
        .unwrap_or_default()
        .into_iter()
        .next()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "meeting not found".to_string()))?;

    let mut sr = state.db
        .query("SELECT seg_index, text, ts_ms, chunk_start_ms FROM meeting_segments WHERE meeting_id = $mid ORDER BY seg_index")
        .bind(("mid", id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let segments: Vec<SegRow> = sr.take(0).unwrap_or_default();

    // Fetch speaker_spans for attribution (SpeakerEngine writes these asynchronously)
    let mut span_r = state.db
        .query("SELECT speaker_id, start_ms, end_ms FROM speaker_spans WHERE meeting_id = $mid ORDER BY start_ms")
        .bind(("mid", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let spans: Vec<SpanRow> = span_r.take(0).unwrap_or_default();

    // Join: for each segment, find the span with most overlap to get the speaker
    let name_map: std::collections::HashMap<String, String> =
        serde_json::from_str(meeting.speaker_names_json.as_deref().unwrap_or("{}")).unwrap_or_default();

    let segments_with_speaker: Vec<serde_json::Value> = segments.iter().map(|seg| {
        let seg_start = seg.chunk_start_ms.unwrap_or(seg.ts_ms);
        let seg_end   = seg_start + 8000; // rough 8s window
        let speaker = spans.iter()
            .filter(|s| s.start_ms < seg_end && s.end_ms > seg_start)
            .map(|s| {
                let overlap = (s.end_ms.min(seg_end) - s.start_ms.max(seg_start)).max(0);
                (overlap, &s.speaker_id)
            })
            .max_by_key(|(o, _)| *o)
            .map(|(_, spk)| name_map.get(spk.as_str()).cloned().unwrap_or_else(|| spk.clone()));

        json!({
            "seg_index": seg.seg_index,
            "text": seg.text,
            "ts_ms": seg.ts_ms,
            "speaker": speaker,
        })
    }).collect();

    Ok(Json(json!({ "meeting": meeting, "segments": segments_with_speaker, "speaker_spans": spans })))
}

#[derive(Deserialize)]
struct RenameSpeakerBody {
    speaker: String,
    name: String,
}

async fn rename_speaker(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<RenameSpeakerBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Fetch current name map
    #[derive(serde::Deserialize)]
    struct Row { speaker_names_json: Option<String> }
    let mut r = state.db
        .query("SELECT speaker_names_json FROM meetings WHERE meeting_id = $mid LIMIT 1")
        .bind(("mid", id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let row = r.take::<Vec<Row>>(0).unwrap_or_default().into_iter().next()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "meeting not found".to_string()))?;

    let current = row.speaker_names_json.unwrap_or_else(|| "{}".to_string());
    let mut map: std::collections::HashMap<String, String> =
        serde_json::from_str(&current).unwrap_or_default();
    map.insert(body.speaker, body.name);
    let updated = serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string());

    state.db
        .query("UPDATE meetings SET speaker_names_json = $names WHERE meeting_id = $mid")
        .bind(("mid", id))
        .bind(("names", updated.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true, "speaker_names_json": updated })))
}

async fn delete_meeting(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Fetch wav_path to clean up file
    #[derive(serde::Deserialize)]
    struct Row { wav_path: Option<String> }
    let mut r = state.db
        .query("SELECT wav_path FROM meetings WHERE meeting_id = $mid LIMIT 1")
        .bind(("mid", id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if let Some(row) = r.take::<Vec<Row>>(0).unwrap_or_default().into_iter().next() {
        if let Some(path) = row.wav_path {
            let _ = std::fs::remove_file(&path);
        }
    }

    state.db
        .query("DELETE FROM meeting_segments WHERE meeting_id = $mid; DELETE FROM speaker_spans WHERE meeting_id = $mid; DELETE FROM meetings WHERE meeting_id = $mid")
        .bind(("mid", id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}
