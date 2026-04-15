use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::app_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/meetings", get(list_meetings))
        .route("/meetings/:id", get(get_meeting).delete(delete_meeting))
        .route("/meetings/:id/rename-speaker", post(rename_speaker))
        .route("/meetings/:id/summarize", post(summarize_meeting))
        .route("/meetings/pre-brief", post(pre_meeting_brief))
        .route("/meetings/participants", get(get_meeting_participants))
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

/// POST /meetings/:id/summarize
/// Manually (re-)trigger the Agent post-process for a meeting.
/// Returns immediately; emits `meeting:summarized` SSE when done.
async fn summarize_meeting(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Verify meeting exists
    #[derive(serde::Deserialize)]
    struct Row { meeting_id: String }
    let mut r = state.db
        .query("SELECT meeting_id FROM meetings WHERE meeting_id = $mid LIMIT 1")
        .bind(("mid", id.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    r.take::<Vec<Row>>(0).unwrap_or_default().into_iter().next()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "meeting not found".to_string()))?;

    let mid = id.clone();
    tokio::spawn(async move {
        use crate::routes::ws_transcribe::{build_meeting_transcript, run_meeting_agent};
        let Some((transcript, date, started_at, vault_id_opt, account_id_opt)) =
            build_meeting_transcript(&state, &mid).await
        else { return; };
        let vault_id = match vault_id_opt.as_deref() {
            Some(v) if !v.is_empty() => v.to_string(),
            _ => return,
        };
        let account_id = account_id_opt.unwrap_or_default();
        if let Some(rel_path) = run_meeting_agent(
            &state, &mid, &transcript, &date, started_at, &vault_id, &account_id,
        ).await {
            let _ = state.db
                .query("UPDATE meetings SET note_path = $path WHERE meeting_id = $mid")
                .bind(("path", rel_path.clone()))
                .bind(("mid", mid.clone()))
                .await;
            state.daemon.emit("meeting:summarized", serde_json::json!({
                "meeting_id": mid,
                "note_path":  rel_path,
            }));
        }
    });

    Ok(Json(json!({ "ok": true, "status": "processing" })))
}

// ─── Pre-meeting brief ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PreBriefBody {
    vault_id:  String,
    topic:     String,
    session_id: Option<String>,
}

const PRE_BRIEF_AGENT_SYSTEM: &str = "\
你是一個「會前情報員」，幫助使用者在開會前快速掌握相關背景資訊。\n\
\n\
使用者會告訴你這次會議的主題。你的工作：\n\
1. 用 search_past_meetings 搜尋與主題相關的歷史會議（keyword 用主題關鍵詞）。\n\
2. 用 list_open_actions 查詢與主題相關的未完成行動項目。\n\
3. 若找到相關歷史，用 get_meeting_context 取得最相關那場會議的完整決策與行動。\n\
4. 選擇性：用 search_vault 搜尋 vault 中是否有相關背景文件。\n\
5. 產出簡報（繁體中文，Markdown，控制在 500 字以內）：\n\
   - **上次相關討論** — 條列上次會議的主要結論（若有）\n\
   - **未完成事項** — 尚未完成的 action items，標明負責人（若有）\n\
   - **相關決策記錄** — 過去做出的相關決策（若有）\n\
   - **相關資料** — vault 中的相關文件（若有）\n\
\n\
若沒有任何相關歷史記錄，直接輸出：「尚無相關歷史記錄，這是第一次討論此主題。」\n\
只輸出簡報內容，不要有其他說明。";

/// POST /meetings/pre-brief
/// Body: { vault_id, topic, session_id? }
/// Spawns a pre-meeting brief Agent (streaming via llm:token events).
/// Returns { session_id } immediately.
async fn pre_meeting_brief(
    State(state): State<ApiState>,
    Json(body): Json<PreBriefBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if body.topic.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "topic is required".to_string()));
    }

    let session_id = body.session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let vault_id   = body.vault_id.clone();
    let topic      = body.topic.clone();
    let sid        = session_id.clone();

    tokio::spawn(async move {
        use serde_json::json;

        // Resolve account_id from vault
        #[derive(serde::Deserialize)]
        struct VaultRow { account_id: String }
        let mut vr = state.db
            .query("SELECT account_id FROM vaults WHERE vault_id = $vid LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .await.ok();
        let account_id = vr.as_mut()
            .and_then(|r| r.take::<Vec<VaultRow>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| r.account_id)
            .unwrap_or_default();

        let agent_def = json!({
            "name": "pre_meeting_brief",
            "kind": "chat",
            "system_prompt": PRE_BRIEF_AGENT_SYSTEM,
            "tool_names": ["search_past_meetings", "list_open_actions", "get_meeting_context", "search_vault"],
            "max_rounds": 8,
            "enable_think": false,
        });

        let conv_id = format!("pre-brief-{}", &sid[..sid.len().min(8)]);
        let runtime = crate::service::build_agent_runtime(
            &state, &vault_id, &account_id,
            Some(sid.clone()), conv_id, agent_def,
            true, Some("zh-TW"),
            Some("pre_brief".to_string()),
            None,
        ).await;

        if let Some(rt) = runtime {
            let prompt = format!("請幫我準備以下主題的會前簡報：{}", topic);
            crate::service::run_agent(rt, prompt, None).await;
        } else {
            state.daemon.emit("llm:done", serde_json::json!({ "t": "" }));
        }
    });

    Ok(Json(json!({ "session_id": session_id })))
}

/// GET /meetings/participants?vault_id=...&topic=...
/// Returns a deduplicated list of participant names from past meetings matching the topic.
#[derive(Deserialize)]
struct ParticipantsQuery {
    vault_id: String,
    topic:    Option<String>,
}

async fn get_meeting_participants(
    State(state): State<ApiState>,
    Query(q): Query<ParticipantsQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    // Fetch recent meetings for this vault (last 90 days)
    let since_ms = chrono::Utc::now().timestamp_millis() - 90 * 86_400_000i64;

    #[derive(serde::Deserialize)]
    struct Row {
        meeting_id: String,
        speaker_names_json: String,
        topic: Option<String>,
    }

    let mut r = state.db
        .query("SELECT meeting_id, speaker_names_json, topic FROM meetings WHERE vault_id = $vid AND started_at >= $since ORDER BY started_at DESC LIMIT 100")
        .bind(("vid", q.vault_id.clone()))
        .bind(("since", since_ms))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Row> = r.take(0).unwrap_or_default();

    let topic_lower = q.topic.as_deref().unwrap_or("").to_lowercase();

    // Collect all unique speaker names, weighting by topic relevance
    let mut names: std::collections::HashMap<String, u32> = Default::default();
    for row in &rows {
        // Topic relevance boost: if meeting topic or segments contain the query
        let is_relevant = topic_lower.is_empty()
            || row.topic.as_deref().unwrap_or("").to_lowercase().contains(&topic_lower);

        let weight: u32 = if is_relevant { 2 } else { 1 };

        let name_map: std::collections::HashMap<String, String> =
            serde_json::from_str(&row.speaker_names_json).unwrap_or_default();
        for name in name_map.values() {
            let n = name.trim();
            if !n.is_empty() && !n.starts_with("SPEAKER_") {
                *names.entry(n.to_string()).or_insert(0) += weight;
            }
        }
    }

    // Sort by relevance score descending
    let mut sorted: Vec<(String, u32)> = names.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    let participants: Vec<String> = sorted.into_iter().take(20).map(|(n, _)| n).collect();

    Ok(Json(json!({ "participants": participants })))
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
