use axum::{
    extract::{Query, State, WebSocketUpgrade},
    extract::ws::{Message, WebSocket},
    response::IntoResponse,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use crate::app_state::ApiState;
use crate::audio_store::AudioStore;
use crate::speaker_engine::{self, SegmentForAttribution, SpeakerEvent};

#[derive(Deserialize)]
pub struct WsQuery {
    token: Option<String>,
}

pub fn router() -> axum::Router<ApiState> {
    axum::Router::new().route("/ws/transcribe", axum::routing::get(ws_transcribe_handler))
}

pub async fn ws_transcribe_handler(
    ws: WebSocketUpgrade,
    State(state): State<ApiState>,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_transcribe(socket, state, query.token))
}

// ─── VAD parameters (16 kHz sample basis) ────────────────────────────────────
const SAMPLE_RATE: usize         = 16_000;
const RMS_THRESHOLD: f32         = 0.015;
const MIN_CHUNK_RMS: f32         = 0.008;
const SILENCE_SAMPLES: usize     = 6_400;    // 400 ms
const MIN_SEGMENT_SAMPLES: usize = 4_800;    // 0.3 s
const MAX_SEGMENT_SAMPLES: usize = SAMPLE_RATE * 8;  // 8 s
const AMBIENT_DRAIN_SAMPLES: usize = SAMPLE_RATE * 5;

const MAX_CONTEXT_CHARS: usize = 200;

fn rms_i16(samples: &[i16]) -> f32 {
    if samples.is_empty() { return 0.0; }
    let sum_sq: f64 = samples.iter()
        .map(|&s| { let f = s as f64 / 32768.0; f * f })
        .sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

struct TranscribeResult {
    index: u32,
    ts_ms: i64,
    chunk_start_ms: u64,
    outcome: Result<crate::routes::whisper::TranscriptResult, String>,
}

fn spawn_transcribe(
    state: ApiState,
    samples: Vec<i16>,
    language: String,
    context: String,
    ts_ms: i64,
    chunk_start_ms: u64,
    index: u32,
    result_tx: mpsc::Sender<TranscribeResult>,
) {
    let mut shutdown_rx = state.daemon.ws_shutdown_tx.subscribe();
    tokio::spawn(async move {
        let ctx = if context.is_empty() { None } else { Some(context.as_str()) };
        let outcome = tokio::select! {
            r = crate::routes::whisper::transcribe_pcm16_verbose(&state, &samples, &language, ctx) => r,
            _ = shutdown_rx.recv() => Err("server shutdown".to_string()),
        };
        let _ = result_tx.send(TranscribeResult { index, ts_ms, chunk_start_ms, outcome }).await;
    });
}

// ─── Meeting persistence helpers ─────────────────────────────────────────────

async fn create_meeting(
    state: &ApiState,
    meeting_id: &str,
    vault_id: &str,
    language: &str,
    account_id: &str,
    topic: Option<&str>,
    parent_meeting_id: Option<&str>,
) {
    let now = Utc::now().timestamp_millis();
    let _ = state.db
        .query("INSERT INTO meetings (meeting_id, vault_id, account_id, language, started_at, status, topic, parent_meeting_id) VALUES ($mid, $vid, $aid, $lang, $now, 'recording', $topic, $pmid)")
        .bind(("mid", meeting_id.to_string()))
        .bind(("vid", if vault_id.is_empty() { None::<String> } else { Some(vault_id.to_string()) }))
        .bind(("aid", account_id.to_string()))
        .bind(("lang", language.to_string()))
        .bind(("now", now))
        .bind(("topic", topic.map(|s| s.to_string())))
        .bind(("pmid", parent_meeting_id.map(|s| s.to_string())))
        .await;
}

async fn persist_segment(
    state: &ApiState,
    meeting_id: &str,
    index: u32,
    text: &str,
    ts_ms: i64,
    chunk_start_ms: u64,
    words_json: Option<&str>,
) {
    let seg_id = format!("{}-{}", meeting_id, index);
    let _ = state.db
        .query("INSERT INTO meeting_segments (seg_id, meeting_id, seg_index, text, ts_ms, chunk_start_ms, words_json) VALUES ($sid, $mid, $idx, $txt, $ts, $cms, $wj)")
        .bind(("sid", seg_id))
        .bind(("mid", meeting_id.to_string()))
        .bind(("idx", index as i64))
        .bind(("txt", text.to_string()))
        .bind(("ts",  ts_ms))
        .bind(("cms", chunk_start_ms as i64))
        .bind(("wj",  words_json.map(|s| s.to_string())))
        .await;
}

async fn finalize_meeting(state: &ApiState, meeting_id: &str, wav_path: Option<&str>) {
    let now = Utc::now().timestamp_millis();
    let _ = state.db
        .query("UPDATE meetings SET status = 'done', ended_at = $now, wav_path = $wav WHERE meeting_id = $mid")
        .bind(("mid", meeting_id.to_string()))
        .bind(("now", now))
        .bind(("wav", wav_path.map(|p| p.to_string())))
        .await;
}

const MEETING_AGENT_SYSTEM: &str = "\
你是一個專業的會議記錄 Agent。你會收到一段已標注說話者的會議逐字稿。\n\
\n\
你的工作流程：\n\
1. 先用 search_past_meetings 搜尋是否有與本次會議相關的歷史會議（依關鍵主題或參與者搜尋）。\n\
   若找到相關歷史會議，用 get_meeting_context 取得其決策和行動項目，並在本次記錄中標注延續關係。\n\
2. 用 search_vault 搜尋 vault 中是否有相關背景資料（專案說明、先前決策文件）。\n\
3. 根據逐字稿產出完整的 Markdown 格式會議記錄，包含以下段落（全繁體中文）：\n\
   - **會議摘要** — 3 到 5 句描述主要內容與結果\n\
   - **參與者** — 條列識別到的說話者\n\
   - **決策記錄** — 本次確定的決定，條列式\n\
   - **行動項目** — 格式：`- [ ] 事項（負責人：XXX）`，無法確認負責人則標 TBD\n\
   - **延續自上次**（若找到歷史會議）— 說明本次哪些決策或行動項目是延續上次的\n\
   - **相關資料**（若有）— `[[筆記名稱]]` wiki-link 格式\n\
4. 最後必須呼叫 save_meeting_extractions 把決策清單和行動項目寫入資料庫。\n\
   decisions 是字串陣列，每條一個決策；actions 是物件陣列 {description, owner}。\n\
\n\
注意：只輸出筆記本文內容，不要加入逐字稿本身（逐字稿會另外附加）。";

/// Build the formatted transcript string with speaker labels and timestamps.
/// Shared between the WebSocket post-process and the REST summarize endpoint.
pub(crate) async fn build_meeting_transcript(state: &ApiState, meeting_id: &str) -> Option<(String, String, i64, Option<String>, Option<String>)> {
    #[derive(serde::Deserialize)]
    struct SegRow { text: String, ts_ms: i64, chunk_start_ms: i64 }
    #[derive(serde::Deserialize)]
    struct SpanRow { speaker_id: String, start_ms: i64, end_ms: i64 }
    #[derive(serde::Deserialize)]
    struct MeetingRow { vault_id: Option<String>, account_id: Option<String>, started_at: i64, speaker_names_json: String }

    let mut r = state.db
        .query("SELECT text, ts_ms, chunk_start_ms FROM meeting_segments WHERE meeting_id = $mid ORDER BY seg_index")
        .bind(("mid", meeting_id.to_string()))
        .await.ok()?;
    let segments: Vec<SegRow> = r.take(0).unwrap_or_default();
    if segments.is_empty() { return None; }

    let mut sr = state.db
        .query("SELECT speaker_id, start_ms, end_ms FROM speaker_spans WHERE meeting_id = $mid ORDER BY start_ms")
        .bind(("mid", meeting_id.to_string()))
        .await.ok()?;
    let spans: Vec<SpanRow> = sr.take(0).unwrap_or_default();

    let mut mr = state.db
        .query("SELECT vault_id, account_id, started_at, speaker_names_json FROM meetings WHERE meeting_id = $mid LIMIT 1")
        .bind(("mid", meeting_id.to_string()))
        .await.ok()?;
    let meeting: MeetingRow = mr.take::<Vec<MeetingRow>>(0).unwrap_or_default().into_iter().next()?;

    let name_map: std::collections::HashMap<String, String> =
        serde_json::from_str(&meeting.speaker_names_json).unwrap_or_default();

    let resolve_speaker = |seg_start_ms: i64, seg_end_ms: i64| -> Option<String> {
        spans.iter()
            .filter(|s| s.start_ms < seg_end_ms && s.end_ms > seg_start_ms)
            .map(|s| {
                let overlap = (s.end_ms.min(seg_end_ms) - s.start_ms.max(seg_start_ms)).max(0);
                (overlap, &s.speaker_id)
            })
            .max_by_key(|(o, _)| *o)
            .map(|(_, spk)| name_map.get(spk.as_str()).cloned().unwrap_or_else(|| spk.clone()))
    };

    let mut transcript = String::new();
    let mut last_speaker = String::new();
    for seg in &segments {
        let seg_end_ms = seg.chunk_start_ms + 8000;
        let speaker = resolve_speaker(seg.chunk_start_ms, seg_end_ms)
            .unwrap_or_else(|| "unknown".to_string());
        let mins = seg.ts_ms / 60_000;
        let secs = (seg.ts_ms % 60_000) / 1000;
        if speaker != last_speaker {
            transcript.push_str(&format!("\n**{}** `{:02}:{:02}`\n", speaker, mins, secs));
            last_speaker = speaker;
        }
        transcript.push_str(&seg.text);
        transcript.push('\n');
    }

    let date = chrono::DateTime::from_timestamp(meeting.started_at / 1000, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| meeting.started_at.to_string());

    Some((transcript, date, meeting.started_at, meeting.vault_id, meeting.account_id))
}

/// Run the meeting Agent and write the resulting note to vault/meetings/.
/// Returns the relative note path on success.
pub(crate) async fn run_meeting_agent(
    state: &ApiState,
    meeting_id: &str,
    transcript: &str,
    date: &str,
    started_at: i64,
    vault_id: &str,
    account_id: &str,
) -> Option<String> {
    use serde_json::json;

    let agent_def = json!({
        "name": "meeting_summarizer",
        "kind": "task",
        "system_prompt": MEETING_AGENT_SYSTEM,
        "tool_names": ["search_past_meetings", "get_meeting_context", "search_vault", "read_note", "save_meeting_extractions"],
        "max_rounds": 12,
        "enable_think": false,
    });
    let conv_id = format!("meeting-{}", &meeting_id[..meeting_id.len().min(8)]);

    let llm_text = match crate::service::build_agent_runtime(
        state, vault_id, account_id,
        None, conv_id, agent_def,
        false, Some("zh-TW"),
        Some("meeting".to_string()),
        Some(meeting_id.to_string()),
    ).await {
        Some(runtime) => {
            let initial_msg = format!(
                "請整理以下會議逐字稿，產出結構化會議記錄：\n\n{}",
                transcript
            );
            tracing::info!("meeting {}: running Agent summarizer", meeting_id);
            crate::service::run_agent(runtime, initial_msg, None).await
        }
        None => {
            // LLM not configured — fall back to simple oneshot
            tracing::warn!("meeting {}: LLM not configured, falling back to oneshot", meeting_id);
            let system = "你是一個會議記錄助理。根據以下逐字稿，產出：\n1. 會議摘要（3–5 句）\n2. 行動項目（Action Items，條列式，含負責人）\n3. 決策記錄（條列式）\n\n請用繁體中文回答，使用 Markdown 格式。";
            match call_llm_oneshot(state, system, &format!("逐字稿：\n{}", transcript), 2048).await {
                Ok(t) => t,
                Err(e) => { tracing::warn!("meeting {}: oneshot failed: {}", meeting_id, e); return None; }
            }
        }
    };

    // Write note file
    let vault_path = state.resolve_vault_path(vault_id).await;
    if vault_path.is_empty() { return None; }

    let meetings_dir = std::path::Path::new(&vault_path).join("meetings");
    let _ = std::fs::create_dir_all(&meetings_dir);
    let filename = format!("{}-{}.md",
        chrono::DateTime::from_timestamp(started_at / 1000, 0)
            .map(|dt| dt.format("%Y%m%d-%H%M").to_string())
            .unwrap_or_else(|| "meeting".to_string()),
        &meeting_id[..meeting_id.len().min(8)],
    );
    let full_path = meetings_dir.join(&filename);
    let note_content = format!(
        "---\ntags: [meeting]\ndate: {}\n---\n\n# 會議記錄 {}\n\n{}\n\n---\n\n## 逐字稿\n\n{}\n",
        date, date, llm_text, transcript
    );
    if std::fs::write(&full_path, &note_content).is_err() {
        tracing::warn!("meeting {}: failed to write note file", meeting_id);
        return None;
    }
    tracing::info!("meeting {}: note written to {}", meeting_id, full_path.display());
    Some(format!("meetings/{}", filename))
}

/// Spawn meeting post-process (Agent summary). Called after SpeakerEngine attribution
/// is complete so all speaker_spans are already in DB.
async fn spawn_meeting_postprocess(state: ApiState, meeting_id: String) {
    tokio::spawn(async move {
        let Some((transcript, date, started_at, vault_id_opt, account_id_opt)) =
            build_meeting_transcript(&state, &meeting_id).await
        else {
            tracing::info!("meeting {}: no segments, skipping post-process", meeting_id);
            return;
        };

        let vault_id = match vault_id_opt.as_deref() {
            Some(v) if !v.is_empty() => v.to_string(),
            _ => { tracing::warn!("meeting {}: no vault_id, skipping note write", meeting_id); return; }
        };
        let account_id = account_id_opt.unwrap_or_default();

        if let Some(rel_path) = run_meeting_agent(
            &state, &meeting_id, &transcript, &date, started_at, &vault_id, &account_id,
        ).await {
            let _ = state.db
                .query("UPDATE meetings SET note_path = $path WHERE meeting_id = $mid")
                .bind(("path", rel_path.clone()))
                .bind(("mid", meeting_id.clone()))
                .await;
            state.daemon.emit("meeting:summarized", serde_json::json!({
                "meeting_id": meeting_id,
                "note_path":  rel_path,
            }));
        }
    });
}

async fn call_llm_oneshot(state: &ApiState, system: &str, user: &str, max_tokens: u64) -> Result<String, String> {
    let base_url = &state.daemon.llm_url;
    let resp = state.daemon.http_client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&json!({
            "messages": [
                {"role": "system", "content": system},
                {"role": "user",   "content": user},
            ],
            "max_tokens": max_tokens,
            "temperature": 0.3,
            "stream": false,
        }))
        .timeout(std::time::Duration::from_secs(300))
        .send().await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("LLM HTTP {}", resp.status()));
    }
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(json["choices"][0]["message"]["content"]
        .as_str().unwrap_or("").trim().to_string())
}

// ─── Main handler ─────────────────────────────────────────────────────────────

async fn handle_ws_transcribe(socket: WebSocket, state: ApiState, token: Option<String>) {
    let account_id = match auth_token(&state, token).await {
        Some(id) => id,
        None => {
            let (mut tx, _) = socket.split();
            let _ = tx.send(Message::Text(
                json!({"event":"whisper:error","data":"unauthorized"}).to_string()
            )).await;
            return;
        }
    };

    let (mut tx, mut rx) = socket.split();
    let mut shutdown_rx = state.daemon.ws_shutdown_tx.subscribe();
    let (result_tx, mut result_rx) = mpsc::channel::<TranscribeResult>(16);

    let mut pending_tasks: u32 = 0;
    let mut next_send_index: u32 = 0;
    let mut result_buf: std::collections::HashMap<u32, TranscribeResult> = std::collections::HashMap::new();
    let mut context_buf = String::new();

    // ─── Per-connection state ─────────────────────────────────────────────────
    let mut language        = "auto".to_string();
    let mut vault_id        = String::new();
    let mut pcm: Vec<i16>  = Vec::new();
    let mut speech_active   = false;
    let mut silence_samples : usize = 0;
    let mut chunk_start     : usize = 0;
    let mut segment_index   : u32   = 0;
    let mut active          = false;
    let mut stopping        = false;
    /// Total samples received since meeting start (monotonically increasing, never reset).
    /// Used to compute chunk_start_ms = total_samples_received_at_chunk_start * 1000 / SAMPLE_RATE
    let mut total_samples_received: u64 = 0;
    let mut meeting_id: Option<String> = None;
    let meeting_start_ms: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

    // SpeakerEngine channel — created on meeting start, consumed on meeting end
    let mut speaker_tx: Option<mpsc::Sender<SpeakerEvent>> = None;
    let mut audio_store: Option<AudioStore> = None;

    loop {
        tokio::select! {
            msg_opt = rx.next() => {
                let msg = match msg_opt {
                    Some(Ok(m)) => m,
                    _ => { pcm.clear(); break; }
                };

                match msg {
                    Message::Text(text) => {
                        let body: serde_json::Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        match body["type"].as_str() {
                            Some("start") => {
                                language = body["language"].as_str().unwrap_or("auto").to_string();
                                vault_id = body["vault_id"].as_str().unwrap_or("").to_string();
                                let topic = body["topic"].as_str().map(|s| s.to_string());
                                let parent_meeting_id = body["parent_meeting_id"].as_str().map(|s| s.to_string());

                                pcm.clear();
                                speech_active   = false;
                                silence_samples = 0;
                                chunk_start     = 0;
                                segment_index   = 0;
                                next_send_index = 0;
                                result_buf.clear();
                                pending_tasks   = 0;
                                context_buf.clear();
                                active          = true;
                                stopping        = false;
                                total_samples_received = 0;

                                // Create meeting record
                                let mid = uuid::Uuid::new_v4().to_string();
                                meeting_start_ms.store(Utc::now().timestamp_millis(), std::sync::atomic::Ordering::Relaxed);
                                create_meeting(&state, &mid, &vault_id, &language, &account_id,
                                    topic.as_deref(), parent_meeting_id.as_deref()).await;

                                // Create AudioStore (WAV file for this meeting)
                                let wav_dir = state.daemon.data_dir.join("meetings");
                                let _ = std::fs::create_dir_all(&wav_dir);
                                let wav_path = wav_dir.join(format!("{}.wav", &mid));
                                let store = match AudioStore::create(&wav_path) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        tracing::warn!("meeting {}: AudioStore create failed: {}", mid, e);
                                        // Proceed without AudioStore; SpeakerEngine won't be spawned
                                        meeting_id = Some(mid.clone());
                                        let _ = tx.send(Message::Text(
                                            json!({"event":"meeting:started","data":{"meeting_id": mid}}).to_string()
                                        )).await;
                                        continue;
                                    }
                                };

                                // Spawn SpeakerEngine if diarize model is available
                                let model_opt = state.daemon.diarize_model.read().ok()
                                    .and_then(|g| g.clone());
                                if let Some(model) = model_opt {
                                    let tx_se = speaker_engine::spawn(state.clone(), store.clone(), model);
                                    speaker_tx = Some(tx_se);
                                }

                                audio_store = Some(store);
                                meeting_id = Some(mid.clone());

                                let _ = tx.send(Message::Text(
                                    json!({"event":"meeting:started","data":{"meeting_id": mid}}).to_string()
                                )).await;
                            }

                            Some("stop") => {
                                if active {
                                    let remaining = pcm[chunk_start..].to_vec();
                                    if remaining.len() >= MIN_SEGMENT_SAMPLES
                                        && rms_i16(&remaining) >= MIN_CHUNK_RMS
                                    {
                                        let ts_ms = Utc::now().timestamp_millis() - meeting_start_ms.load(std::sync::atomic::Ordering::Relaxed);
                                        let chunk_start_ms = (total_samples_received.saturating_sub(remaining.len() as u64))
                                            * 1000 / SAMPLE_RATE as u64;
                                        spawn_transcribe(
                                            state.clone(), remaining, language.clone(),
                                            context_buf.clone(), ts_ms, chunk_start_ms,
                                            segment_index, result_tx.clone(),
                                        );
                                        segment_index += 1;
                                        pending_tasks += 1;
                                    }
                                    active   = false;
                                    stopping = true;
                                    pcm.clear();
                                    chunk_start     = 0;
                                    speech_active   = false;
                                    silence_samples = 0;

                                    if pending_tasks == 0 {
                                        // Phase 1 done immediately (no pending tasks)
                                        finish_meeting(&state, &mut tx, &mut meeting_id, &mut speaker_tx, &mut audio_store).await;
                                        stopping = false;
                                    }
                                }
                            }

                            Some("cancel") => {
                                active          = false;
                                stopping        = false;
                                pcm.clear();
                                // Drop SpeakerEngine channel (will cause it to exit)
                                drop(speaker_tx.take());
                                if let Some(store) = audio_store.take() {
                                    store.finalize();
                                }
                                if let Some(mid) = &meeting_id {
                                    let _ = state.db
                                        .query("UPDATE meetings SET status = 'cancelled', ended_at = $now WHERE meeting_id = $mid")
                                        .bind(("mid", mid.clone()))
                                        .bind(("now", Utc::now().timestamp_millis()))
                                        .await;
                                }
                                meeting_id = None;
                                chunk_start     = 0;
                                speech_active   = false;
                                silence_samples = 0;
                                result_buf.clear();
                                pending_tasks   = 0;
                                next_send_index = segment_index;
                            }

                            Some("rename_speaker") => {
                                if let (Some(mid), Some(spk), Some(name)) = (
                                    &meeting_id,
                                    body["speaker"].as_str(),
                                    body["name"].as_str(),
                                ) {
                                    update_speaker_name(&state, mid, spk, name).await;
                                }
                            }

                            _ => {}
                        }
                    }

                    Message::Binary(data) => {
                        if !active { continue; }

                        let new_samples: Vec<i16> = data.chunks_exact(2)
                            .map(|b| i16::from_le_bytes([b[0], b[1]]))
                            .collect();
                        if new_samples.is_empty() { continue; }

                        // Commit to AudioStore BEFORE processing (write-before-evict invariant)
                        if let Some(ref store) = audio_store {
                            store.commit(&new_samples);
                        }
                        total_samples_received += new_samples.len() as u64;

                        let frame_rms = rms_i16(&new_samples);
                        let speaking  = frame_rms > RMS_THRESHOLD;

                        pcm.extend_from_slice(&new_samples);

                        if speaking {
                            silence_samples = 0;
                            speech_active   = true;

                            let segment_len = pcm.len() - chunk_start;
                            if segment_len >= MAX_SEGMENT_SAMPLES {
                                let chunk = pcm[chunk_start..].to_vec();
                                if rms_i16(&chunk) >= MIN_CHUNK_RMS {
                                    let ts_ms = Utc::now().timestamp_millis() - meeting_start_ms.load(std::sync::atomic::Ordering::Relaxed);
                                    let chunk_start_ms = (total_samples_received.saturating_sub(chunk.len() as u64))
                                        * 1000 / SAMPLE_RATE as u64;
                                    spawn_transcribe(
                                        state.clone(), chunk, language.clone(),
                                        context_buf.clone(), ts_ms, chunk_start_ms,
                                        segment_index, result_tx.clone(),
                                    );
                                    segment_index += 1;
                                    pending_tasks += 1;
                                }
                                chunk_start   = pcm.len();
                                speech_active = false;
                                silence_samples = 0;
                                pcm.drain(0..chunk_start);
                                chunk_start = 0;
                            }
                        } else {
                            silence_samples += new_samples.len();

                            if speech_active && silence_samples >= SILENCE_SAMPLES {
                                speech_active   = false;
                                silence_samples = 0;

                                let end_idx = pcm.len();
                                let chunk   = &pcm[chunk_start..end_idx];

                                if chunk.len() >= MIN_SEGMENT_SAMPLES && rms_i16(chunk) >= MIN_CHUNK_RMS {
                                    let chunk_owned = chunk.to_vec();
                                    let ts_ms = Utc::now().timestamp_millis() - meeting_start_ms.load(std::sync::atomic::Ordering::Relaxed);
                                    let chunk_start_ms = (total_samples_received.saturating_sub(chunk_owned.len() as u64))
                                        * 1000 / SAMPLE_RATE as u64;
                                    spawn_transcribe(
                                        state.clone(), chunk_owned, language.clone(),
                                        context_buf.clone(), ts_ms, chunk_start_ms,
                                        segment_index, result_tx.clone(),
                                    );
                                    segment_index += 1;
                                    pending_tasks += 1;
                                }

                                chunk_start = end_idx;
                                if chunk_start > SAMPLE_RATE * 60 {
                                    pcm.drain(0..chunk_start);
                                    chunk_start = 0;
                                }
                            } else if !speech_active && (pcm.len() - chunk_start) > AMBIENT_DRAIN_SAMPLES {
                                chunk_start = pcm.len();
                                pcm.drain(0..chunk_start);
                                chunk_start = 0;
                            }
                        }
                    }

                    Message::Close(_) => { pcm.clear(); break; }
                    _ => continue,
                }
            }

            Some(res) = result_rx.recv() => {
                if pending_tasks > 0 { pending_tasks -= 1; }

                result_buf.insert(res.index, res);

                // Emit segments in order (sequence reorder buffer)
                while let Some(res) = result_buf.remove(&next_send_index) {
                    match res.outcome {
                        Ok(transcript) if !transcript.text.is_empty() => {
                            // Update rolling context
                            context_buf.push_str(&transcript.text);
                            if context_buf.chars().count() > MAX_CONTEXT_CHARS {
                                let skip = context_buf.char_indices()
                                    .nth(context_buf.chars().count() - MAX_CONTEXT_CHARS)
                                    .map(|(i, _)| i).unwrap_or(0);
                                context_buf.drain(0..skip);
                            }

                            // Serialize words for DB storage
                            let words_json = if transcript.words.is_empty() {
                                None
                            } else {
                                serde_json::to_string(&transcript.words).ok()
                            };

                            // Persist to DB (TranscriptionEngine; never touches speaker)
                            if let Some(mid) = &meeting_id {
                                persist_segment(
                                    &state, mid, next_send_index, &transcript.text,
                                    res.ts_ms, res.chunk_start_ms, words_json.as_deref(),
                                ).await;

                                // Send to SpeakerEngine via try_send (non-blocking).
                                // We must NOT use send().await here: this runs inside
                                // the tokio::select! result arm, which is the single
                                // event loop for the WebSocket session. Awaiting on a
                                // full channel would block PCM ingestion, stop/cancel
                                // handling, and shutdown — effectively freezing the session.
                                //
                                // Channel capacity is 64. Overflow requires ≥ 64 segments
                                // of LLM backlog (≈ 8+ min with a very slow local LLM).
                                // In that edge case, attribution for this segment is skipped;
                                // the transcript is already persisted to DB.
                                if let Some(ref se_tx) = speaker_tx {
                                    let seg = SegmentForAttribution {
                                        seg_id: format!("{}-{}", mid, next_send_index),
                                        meeting_id: mid.clone(),
                                        seg_index: next_send_index,
                                        text: transcript.text.clone(),
                                        words: transcript.words.clone(),
                                        chunk_start_ms: res.chunk_start_ms,
                                    };
                                    if let Err(e) = se_tx.try_send(SpeakerEvent::SegmentReady(seg)) {
                                        tracing::warn!("SpeakerEngine channel full/closed, seg {} attribution skipped: {}", next_send_index, e);
                                    }
                                }
                            }

                            let data = json!({
                                "text": transcript.text,
                                "index": next_send_index,
                                "ts_ms": res.ts_ms,
                            });
                            let out = json!({"event":"whisper:done","data": data}).to_string();
                            if tx.send(Message::Text(out)).await.is_err() {
                                pcm.clear(); return;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Message::Text(
                                json!({"event":"whisper:error","data":e}).to_string()
                            )).await;
                        }
                        _ => {}
                    }
                    next_send_index += 1;
                }

                // Phase 1 complete: all pending Whisper tasks done after stop
                if stopping && pending_tasks == 0 {
                    let _ = tx.send(Message::Text(
                        json!({"event":"whisper:flush_done"}).to_string()
                    )).await;
                    stopping = false;
                    finish_meeting(&state, &mut tx, &mut meeting_id, &mut speaker_tx, &mut audio_store).await;
                }
            }

            _ = shutdown_rx.recv() => {
                pcm.clear();
                let _ = tx.send(Message::Close(None)).await;
                break;
            }
        }
    }
}

/// Two-phase meeting shutdown:
/// Phase 1: Whisper tasks already flushed (caller guarantees pending_tasks == 0)
/// Phase 2: Send MeetingEnd to SpeakerEngine, await AttributionComplete (30s timeout)
/// Phase 3: Finalize AudioStore WAV, run PostProcess
async fn finish_meeting(
    state: &ApiState,
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    meeting_id: &mut Option<String>,
    speaker_tx: &mut Option<mpsc::Sender<SpeakerEvent>>,
    audio_store: &mut Option<AudioStore>,
) {
    let mid = match meeting_id.take() {
        Some(m) => m,
        None => return,
    };

    // Phase 2: drain SpeakerEngine
    if let Some(se_tx) = speaker_tx.take() {
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let _ = se_tx.send(SpeakerEvent::MeetingEnd(done_tx)).await;
        // Wait up to 30s for attribution to complete
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), done_rx).await;
    }

    // Phase 3: finalize WAV
    let wav_path = if let Some(store) = audio_store.take() {
        store.finalize();
        let meetings_dir = state.daemon.data_dir.join("meetings");
        let p = meetings_dir.join(format!("{}.wav", &mid));
        Some(p.to_string_lossy().to_string())
    } else {
        None
    };

    finalize_meeting(state, &mid, wav_path.as_deref()).await;
    spawn_meeting_postprocess(state.clone(), mid.clone()).await;

    let _ = tx.send(Message::Text(
        json!({"event":"meeting:done","data":{"meeting_id": mid}}).to_string()
    )).await;
}

// ─── Auth helper ──────────────────────────────────────────────────────────────

async fn auth_token(state: &ApiState, token: Option<String>) -> Option<String> {
    let tok = token?;
    let now = Utc::now().timestamp();
    #[derive(serde::Deserialize)]
    struct Row { username: String }
    state.db
        .query("SELECT username FROM sessions WHERE token = $t AND expires_at > $now LIMIT 1")
        .bind(("t", tok))
        .bind(("now", now))
        .await
        .ok()?
        .take::<Vec<Row>>(0)
        .ok()?
        .into_iter()
        .next()
        .map(|r| r.username)
}

// ─── Speaker name helpers ─────────────────────────────────────────────────────

async fn update_speaker_name(state: &ApiState, meeting_id: &str, speaker: &str, name: &str) {
    #[derive(serde::Deserialize)]
    struct Row { speaker_names_json: String }
    let mut r = match state.db
        .query("SELECT speaker_names_json FROM meetings WHERE meeting_id = $mid LIMIT 1")
        .bind(("mid", meeting_id.to_string()))
        .await {
        Ok(r) => r,
        Err(_) => return,
    };
    let current: String = r.take::<Vec<Row>>(0).ok()
        .and_then(|v| v.into_iter().next())
        .map(|r| r.speaker_names_json)
        .unwrap_or_else(|| "{}".to_string());

    let mut map: std::collections::HashMap<String, String> =
        serde_json::from_str(&current).unwrap_or_default();
    map.insert(speaker.to_string(), name.to_string());
    let updated = serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string());

    let _ = state.db
        .query("UPDATE meetings SET speaker_names_json = $names WHERE meeting_id = $mid")
        .bind(("mid", meeting_id.to_string()))
        .bind(("names", updated))
        .await;
}
