use axum::{
    extract::{Query, State, WebSocketUpgrade},
    extract::ws::{Message, WebSocket},
    response::IntoResponse,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::app_state::ApiState;
use crate::diarize::SpeakerTracker;

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
const MAX_SEGMENT_SAMPLES: usize = SAMPLE_RATE * 30; // 30 s
const AMBIENT_DRAIN_SAMPLES: usize = SAMPLE_RATE * 5;

// ~115 MB for 1 h @ 16 kHz i16 — cap at 2 h to avoid runaway memory
const MAX_PCM_ALL_SAMPLES: usize = SAMPLE_RATE * 3600 * 2;

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
    speaker: Option<String>,
    ts_ms: i64,
    outcome: Result<String, String>,
}

fn spawn_transcribe(
    state: ApiState,
    samples: Vec<i16>,
    language: String,
    context: String,
    speaker: Option<String>,
    ts_ms: i64,
    index: u32,
    result_tx: mpsc::Sender<TranscribeResult>,
) {
    let mut shutdown_rx = state.daemon.ws_shutdown_tx.subscribe();
    tokio::spawn(async move {
        let ctx = if context.is_empty() { None } else { Some(context.as_str()) };
        let outcome = tokio::select! {
            r = crate::routes::whisper::transcribe_pcm16(&state, &samples, &language, ctx) => r,
            _ = shutdown_rx.recv() => Err("server shutdown".to_string()),
        };
        let _ = result_tx.send(TranscribeResult { index, speaker, ts_ms, outcome }).await;
    });
}

fn identify_speaker(state: &ApiState, tracker: &mut SpeakerTracker, samples: &[i16]) -> Option<String> {
    let model = {
        let guard = state.daemon.diarize_model.read().ok()?;
        guard.clone()?
    };
    match model.extract(samples) {
        Ok(embedding) => Some(tracker.identify(&embedding)),
        Err(_) => None,
    }
}

// ─── Meeting persistence helpers ─────────────────────────────────────────────

async fn create_meeting(state: &ApiState, meeting_id: &str, vault_id: &str, language: &str, account_id: &str) {
    let now = Utc::now().timestamp_millis();
    let _ = state.db
        .query("INSERT INTO meetings (meeting_id, vault_id, account_id, language, started_at, status) VALUES ($mid, $vid, $aid, $lang, $now, 'recording')")
        .bind(("mid", meeting_id.to_string()))
        .bind(("vid", if vault_id.is_empty() { None::<String> } else { Some(vault_id.to_string()) }))
        .bind(("aid", account_id.to_string()))
        .bind(("lang", language.to_string()))
        .bind(("now", now))
        .await;
}

async fn persist_segment(state: &ApiState, meeting_id: &str, index: u32, speaker: Option<&str>, text: &str, ts_ms: i64) {
    let seg_id = format!("{}-{}", meeting_id, index);
    let _ = state.db
        .query("INSERT INTO meeting_segments (seg_id, meeting_id, seg_index, speaker, text, ts_ms) VALUES ($sid, $mid, $idx, $spk, $txt, $ts)")
        .bind(("sid", seg_id))
        .bind(("mid", meeting_id.to_string()))
        .bind(("idx", index as i64))
        .bind(("spk", speaker.map(|s| s.to_string())))
        .bind(("txt", text.to_string()))
        .bind(("ts", ts_ms))
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

/// Build a 44-byte WAV header + i16 PCM body.
fn build_wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    let write_u32 = |buf: &mut Vec<u8>, v: u32| buf.extend_from_slice(&v.to_le_bytes());
    let write_u16 = |buf: &mut Vec<u8>, v: u16| buf.extend_from_slice(&v.to_le_bytes());
    wav.extend_from_slice(b"RIFF");
    write_u32(&mut wav, 36 + data_len);
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    write_u32(&mut wav, 16);
    write_u16(&mut wav, 1); // PCM
    write_u16(&mut wav, 1); // mono
    write_u32(&mut wav, sample_rate);
    write_u32(&mut wav, sample_rate * 2);
    write_u16(&mut wav, 2); // block align
    write_u16(&mut wav, 16); // bits/sample
    wav.extend_from_slice(b"data");
    write_u32(&mut wav, data_len);
    for &s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

/// Spawn a background task that:
/// 1. Fetches all segments for the meeting from DB
/// 2. Calls LLM to generate a summary note
/// 3. Writes the note to the vault
/// 4. Updates meeting.note_path
async fn spawn_meeting_postprocess(state: ApiState, meeting_id: String) {
    tokio::spawn(async move {
        #[derive(serde::Deserialize)]
        struct SegRow { seg_index: i64, speaker: Option<String>, text: String, ts_ms: i64 }
        #[derive(serde::Deserialize)]
        struct MeetingRow { vault_id: Option<String>, language: String, started_at: i64, speaker_names_json: String }

        let mut r = match state.db
            .query("SELECT seg_index, speaker, text, ts_ms FROM meeting_segments WHERE meeting_id = $mid ORDER BY seg_index")
            .bind(("mid", meeting_id.clone()))
            .await {
            Ok(r) => r,
            Err(e) => { tracing::warn!("meeting post-process: fetch segments failed: {}", e); return; }
        };
        let segments: Vec<SegRow> = r.take(0).unwrap_or_default();

        if segments.is_empty() {
            tracing::info!("meeting {}: no segments, skipping post-process", meeting_id);
            return;
        }

        let mut mr = match state.db
            .query("SELECT vault_id, language, started_at, speaker_names_json FROM meetings WHERE meeting_id = $mid LIMIT 1")
            .bind(("mid", meeting_id.clone()))
            .await {
            Ok(r) => r,
            Err(_) => return,
        };
        let meeting: MeetingRow = match mr.take::<Vec<MeetingRow>>(0).ok().and_then(|v| v.into_iter().next()) {
            Some(m) => m,
            None => return,
        };

        // Parse speaker name map {"SPEAKER_00": "Alice"}
        let name_map: std::collections::HashMap<String, String> =
            serde_json::from_str(&meeting.speaker_names_json).unwrap_or_default();

        let resolve_name = |spk: &Option<String>| -> String {
            match spk {
                None => "unknown".to_string(),
                Some(s) => name_map.get(s).cloned().unwrap_or_else(|| s.clone()),
            }
        };

        // Build transcript block
        let mut transcript = String::new();
        let mut last_speaker = String::new();
        for seg in &segments {
            let name = resolve_name(&seg.speaker);
            let mins = seg.ts_ms / 60_000;
            let secs = (seg.ts_ms % 60_000) / 1000;
            if name != last_speaker {
                transcript.push_str(&format!("\n**{}** `{:02}:{:02}`\n", name, mins, secs));
                last_speaker = name.clone();
            }
            transcript.push_str(&seg.text);
            transcript.push('\n');
        }

        // LLM prompt
        let system = "你是一個會議記錄助理。根據以下逐字稿，產出：\n1. 會議摘要（3–5 句）\n2. 行動項目（Action Items，條列式，含負責人）\n3. 決策記錄（條列式）\n\n請用繁體中文回答，使用 Markdown 格式。";
        let user_content = format!("逐字稿：\n{}", transcript);

        // Call LLM
        let llm_result = call_llm_oneshot(&state, system, &user_content, 2048).await;
        let llm_text = match llm_result {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("meeting {}: LLM call failed: {}", meeting_id, e);
                format!("*LLM 摘要失敗：{}*\n", e)
            }
        };

        // Build markdown note
        let date = chrono::DateTime::from_timestamp(meeting.started_at / 1000, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| meeting.started_at.to_string());

        let note_content = format!(
            "---\ntags: [meeting]\ndate: {}\n---\n\n# 會議記錄 {}\n\n{}\n\n---\n\n## 逐字稿\n\n{}\n",
            date, date, llm_text, transcript
        );

        // Write note to vault
        let note_path = if let Some(vault_id) = &meeting.vault_id {
            let vault_path = state.resolve_vault_path(vault_id).await;
            if !vault_path.is_empty() {
                let meetings_dir = std::path::Path::new(&vault_path).join("meetings");
                let _ = std::fs::create_dir_all(&meetings_dir);
                let filename = format!("{}-{}.md",
                    chrono::DateTime::from_timestamp(meeting.started_at / 1000, 0)
                        .map(|dt| dt.format("%Y%m%d-%H%M").to_string())
                        .unwrap_or_else(|| "meeting".to_string()),
                    &meeting_id[..8],
                );
                let full_path = meetings_dir.join(&filename);
                match std::fs::write(&full_path, &note_content) {
                    Ok(_) => {
                        let rel = format!("meetings/{}", filename);
                        // Update note_path in DB
                        let _ = state.db
                            .query("UPDATE meetings SET note_path = $path WHERE meeting_id = $mid")
                            .bind(("path", rel.clone()))
                            .bind(("mid", meeting_id.clone()))
                            .await;
                        tracing::info!("meeting {}: note written to {}", meeting_id, full_path.display());
                        Some(rel)
                    }
                    Err(e) => {
                        tracing::warn!("meeting {}: write note failed: {}", meeting_id, e);
                        None
                    }
                }
            } else { None }
        } else { None };

        tracing::info!("meeting {} post-processing complete, note={:?}", meeting_id, note_path);
    });
}

/// Non-streaming LLM call — returns the assistant message text.
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
    let mut pcm_all: Vec<i16> = Vec::new(); // full recording for WAV export
    let mut speech_active   = false;
    let mut silence_samples : usize = 0;
    let mut chunk_start     : usize = 0;
    let mut segment_index   : u32   = 0;
    let mut active          = false;
    let mut stopping        = false;
    let mut speaker_tracker = SpeakerTracker::new();
    let mut meeting_id: Option<String> = None;
    let meeting_start_ms: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

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

                                pcm.clear();
                                pcm_all.clear();
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
                                speaker_tracker = SpeakerTracker::new();

                                // Create meeting record
                                let mid = uuid::Uuid::new_v4().to_string();
                                meeting_start_ms.store(Utc::now().timestamp_millis(), std::sync::atomic::Ordering::Relaxed);
                                create_meeting(&state, &mid, &vault_id, &language, &account_id).await;
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
                                        let speaker = identify_speaker(&state, &mut speaker_tracker, &remaining);
                                        let ts_ms = Utc::now().timestamp_millis() - meeting_start_ms.load(std::sync::atomic::Ordering::Relaxed);
                                        spawn_transcribe(
                                            state.clone(), remaining, language.clone(),
                                            context_buf.clone(), speaker, ts_ms,
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
                                        let _ = tx.send(Message::Text(
                                            json!({"event":"whisper:flush_done"}).to_string()
                                        )).await;
                                        stopping = false;
                                        if let Some(mid) = &meeting_id {
                                            let wav_path = save_wav(&state, mid, &pcm_all);
                                            finalize_meeting(&state, mid, wav_path.as_deref()).await;
                                            spawn_meeting_postprocess(state.clone(), mid.clone()).await;
                                        }
                                    }
                                }
                            }

                            Some("cancel") => {
                                active          = false;
                                stopping        = false;
                                pcm.clear();
                                pcm_all.clear();
                                chunk_start     = 0;
                                speech_active   = false;
                                silence_samples = 0;
                                result_buf.clear();
                                pending_tasks   = 0;
                                next_send_index = segment_index;
                                // Mark meeting as cancelled
                                if let Some(mid) = &meeting_id {
                                    let _ = state.db
                                        .query("UPDATE meetings SET status = 'cancelled', ended_at = $now WHERE meeting_id = $mid")
                                        .bind(("mid", mid.clone()))
                                        .bind(("now", Utc::now().timestamp_millis()))
                                        .await;
                                }
                                meeting_id = None;
                            }

                            Some("rename_speaker") => {
                                // {"type":"rename_speaker","speaker":"SPEAKER_00","name":"Alice"}
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

                        let frame_rms = rms_i16(&new_samples);
                        let speaking  = frame_rms > RMS_THRESHOLD;

                        pcm.extend_from_slice(&new_samples);
                        // Accumulate full recording (capped)
                        if pcm_all.len() < MAX_PCM_ALL_SAMPLES {
                            let remaining_cap = MAX_PCM_ALL_SAMPLES - pcm_all.len();
                            let take = new_samples.len().min(remaining_cap);
                            pcm_all.extend_from_slice(&new_samples[..take]);
                        }

                        if speaking {
                            silence_samples = 0;
                            speech_active   = true;

                            let segment_len = pcm.len() - chunk_start;
                            if segment_len >= MAX_SEGMENT_SAMPLES {
                                let chunk = pcm[chunk_start..].to_vec();
                                if rms_i16(&chunk) >= MIN_CHUNK_RMS {
                                    let speaker = identify_speaker(&state, &mut speaker_tracker, &chunk);
                                    let ts_ms = Utc::now().timestamp_millis() - meeting_start_ms.load(std::sync::atomic::Ordering::Relaxed);
                                    spawn_transcribe(
                                        state.clone(), chunk, language.clone(),
                                        context_buf.clone(), speaker, ts_ms,
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
                                    let speaker = identify_speaker(&state, &mut speaker_tracker, &chunk_owned);
                                    let ts_ms = Utc::now().timestamp_millis() - meeting_start_ms.load(std::sync::atomic::Ordering::Relaxed);
                                    spawn_transcribe(
                                        state.clone(), chunk_owned, language.clone(),
                                        context_buf.clone(), speaker, ts_ms,
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
                while let Some(res) = result_buf.remove(&next_send_index) {
                    match res.outcome {
                        Ok(text) if !text.is_empty() => {
                            // Update rolling context
                            context_buf.push_str(&text);
                            if context_buf.chars().count() > MAX_CONTEXT_CHARS {
                                let skip = context_buf.char_indices()
                                    .nth(context_buf.chars().count() - MAX_CONTEXT_CHARS)
                                    .map(|(i, _)| i).unwrap_or(0);
                                context_buf.drain(0..skip);
                            }

                            // Persist to DB
                            if let Some(mid) = &meeting_id {
                                persist_segment(&state, mid, next_send_index, res.speaker.as_deref(), &text, res.ts_ms).await;
                            }

                            let mut data = json!({
                                "text": text,
                                "index": next_send_index,
                                "ts_ms": res.ts_ms,
                            });
                            if let Some(spk) = &res.speaker {
                                data["speaker"] = json!(spk);
                            }
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

                if stopping && pending_tasks == 0 {
                    let _ = tx.send(Message::Text(
                        json!({"event":"whisper:flush_done"}).to_string()
                    )).await;
                    stopping = false;

                    if let Some(mid) = &meeting_id {
                        let wav_path = save_wav(&state, mid, &pcm_all);
                        finalize_meeting(&state, mid, wav_path.as_deref()).await;
                        spawn_meeting_postprocess(state.clone(), mid.clone()).await;
                    }
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

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn save_wav(state: &ApiState, meeting_id: &str, pcm_all: &[i16]) -> Option<String> {
    if pcm_all.is_empty() { return None; }
    let meetings_dir = state.daemon.data_dir.join("meetings");
    if std::fs::create_dir_all(&meetings_dir).is_err() { return None; }
    let path = meetings_dir.join(format!("{}.wav", meeting_id));
    let wav = build_wav(pcm_all, SAMPLE_RATE as u32);
    match std::fs::write(&path, wav) {
        Ok(_) => {
            tracing::info!("meeting {}: WAV saved to {}", meeting_id, path.display());
            Some(path.to_string_lossy().to_string())
        }
        Err(e) => {
            tracing::warn!("meeting {}: WAV save failed: {}", meeting_id, e);
            None
        }
    }
}

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
