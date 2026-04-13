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

// ─── VAD parameters — mirror frontend constants (16 kHz sample basis) ─────────
const SAMPLE_RATE: usize         = 16_000;
const RMS_THRESHOLD: f32         = 0.015;
const MIN_CHUNK_RMS: f32         = 0.008;
const SILENCE_SAMPLES: usize     = 6_400;    // 400 ms × 16000 Hz
const MIN_SEGMENT_SAMPLES: usize = 4_800;    // 0.3 s  × 16000 Hz
const MAX_SEGMENT_SAMPLES: usize = SAMPLE_RATE * 30; // 30 s hard cap
const AMBIENT_DRAIN_SAMPLES: usize = SAMPLE_RATE * 5;

/// RMS of i16 PCM samples, normalised to [-1, 1] float scale.
fn rms_i16(samples: &[i16]) -> f32 {
    if samples.is_empty() { return 0.0; }
    let sum_sq: f64 = samples.iter()
        .map(|&s| { let f = s as f64 / 32768.0; f * f })
        .sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

// Maximum chars kept as rolling context passed to next whisper call.
const MAX_CONTEXT_CHARS: usize = 200;

/// Result of one transcription job, forwarded back to the sender loop.
struct TranscribeResult {
    index: u32,
    speaker: Option<String>,
    outcome: Result<String, String>,
}

/// Spawn a transcription task. Results are sent through `result_tx`.
fn spawn_transcribe(
    state: ApiState,
    samples: Vec<i16>,
    language: String,
    context: String,
    speaker: Option<String>,
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
        let _ = result_tx.send(TranscribeResult { index, speaker, outcome }).await;
    });
}

/// Extract speaker embedding synchronously (CPU, <20ms) and identify speaker.
/// Returns None if no diarize model is loaded.
fn identify_speaker(state: &ApiState, tracker: &mut SpeakerTracker, samples: &[i16]) -> Option<String> {
    let model = state.daemon.diarize_model.as_ref()?;
    match model.extract(samples) {
        Ok(embedding) => Some(tracker.identify(&embedding)),
        Err(_) => None,
    }
}

async fn handle_ws_transcribe(socket: WebSocket, state: ApiState, token: Option<String>) {
    // Auth once on connect
    let _account_id = match auth_token(&state, token).await {
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

    // Channel for transcription results from background tasks
    let (result_tx, mut result_rx) = mpsc::channel::<TranscribeResult>(16);

    let mut pending_tasks: u32 = 0;
    let mut next_send_index: u32 = 0;
    let mut result_buf: std::collections::HashMap<u32, TranscribeResult> = std::collections::HashMap::new();
    let mut context_buf = String::new();

    // ─── Per-connection state ──────────────────────────────────────────────────
    let mut language        = "auto".to_string();
    let mut pcm: Vec<i16>  = Vec::new();
    let mut speech_active   = false;
    let mut silence_samples : usize = 0;
    let mut chunk_start     : usize = 0;
    let mut segment_index   : u32   = 0;
    let mut active          = false;
    let mut stopping        = false;
    // Per-connection speaker tracker (reset on each "start")
    let mut speaker_tracker = SpeakerTracker::new();

    loop {
        tokio::select! {
            // ── Incoming WS message ───────────────────────────────────────────
            msg_opt = rx.next() => {
                let msg = match msg_opt {
                    Some(Ok(m)) => m,
                    _ => {
                        pcm.clear();
                        break;
                    }
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
                                speaker_tracker = SpeakerTracker::new();
                            }

                            Some("stop") => {
                                if active {
                                    let remaining = pcm[chunk_start..].to_vec();
                                    if remaining.len() >= MIN_SEGMENT_SAMPLES
                                        && rms_i16(&remaining) >= MIN_CHUNK_RMS
                                    {
                                        let speaker = identify_speaker(&state, &mut speaker_tracker, &remaining);
                                        spawn_transcribe(
                                            state.clone(), remaining, language.clone(),
                                            context_buf.clone(), speaker, segment_index, result_tx.clone(),
                                        );
                                        segment_index += 1;
                                        pending_tasks += 1;
                                    }
                                    active  = false;
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
                                    }
                                }
                            }

                            Some("cancel") => {
                                active          = false;
                                stopping        = false;
                                pcm.clear();
                                chunk_start     = 0;
                                speech_active   = false;
                                silence_samples = 0;
                                result_buf.clear();
                                pending_tasks = 0;
                                next_send_index = segment_index;
                            }

                            _ => {}
                        }
                    }

                    // ── Binary: PCM16 frames ──────────────────────────────────
                    Message::Binary(data) => {
                        if !active { continue; }

                        let new_samples: Vec<i16> = data.chunks_exact(2)
                            .map(|b| i16::from_le_bytes([b[0], b[1]]))
                            .collect();
                        if new_samples.is_empty() { continue; }

                        let frame_rms = rms_i16(&new_samples);
                        let speaking  = frame_rms > RMS_THRESHOLD;

                        pcm.extend_from_slice(&new_samples);

                        if speaking {
                            silence_samples = 0;
                            speech_active   = true;

                            // ── Max segment cap: force flush at 30 s ──────────
                            let segment_len = pcm.len() - chunk_start;
                            if segment_len >= MAX_SEGMENT_SAMPLES {
                                let chunk = pcm[chunk_start..].to_vec();
                                if rms_i16(&chunk) >= MIN_CHUNK_RMS {
                                    let speaker = identify_speaker(&state, &mut speaker_tracker, &chunk);
                                    spawn_transcribe(
                                        state.clone(), chunk, language.clone(),
                                        context_buf.clone(), speaker, segment_index, result_tx.clone(),
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
                                // ── VAD: silence detected, flush segment ──────
                                speech_active   = false;
                                silence_samples = 0;

                                let end_idx = pcm.len();
                                let chunk   = &pcm[chunk_start..end_idx];

                                if chunk.len() >= MIN_SEGMENT_SAMPLES
                                    && rms_i16(chunk) >= MIN_CHUNK_RMS
                                {
                                    let chunk_owned = chunk.to_vec();
                                    let speaker = identify_speaker(&state, &mut speaker_tracker, &chunk_owned);
                                    spawn_transcribe(
                                        state.clone(), chunk_owned, language.clone(),
                                        context_buf.clone(), speaker, segment_index, result_tx.clone(),
                                    );
                                    segment_index += 1;
                                    pending_tasks += 1;
                                }

                                chunk_start = end_idx;

                                if chunk_start > SAMPLE_RATE * 60 {
                                    pcm.drain(0..chunk_start);
                                    chunk_start = 0;
                                }
                            } else if !speech_active
                                && (pcm.len() - chunk_start) > AMBIENT_DRAIN_SAMPLES
                            {
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

            // ── Transcription result from background task ─────────────────────
            Some(res) = result_rx.recv() => {
                if pending_tasks > 0 { pending_tasks -= 1; }

                // Buffer out-of-order results; forward in-order
                result_buf.insert(res.index, res);
                while let Some(res) = result_buf.remove(&next_send_index) {
                    match res.outcome {
                        Ok(text) if !text.is_empty() => {
                            // Update rolling context
                            context_buf.push_str(&text);
                            if context_buf.chars().count() > MAX_CONTEXT_CHARS {
                                let skip = context_buf.char_indices()
                                    .nth(context_buf.chars().count() - MAX_CONTEXT_CHARS)
                                    .map(|(i, _)| i)
                                    .unwrap_or(0);
                                context_buf.drain(0..skip);
                            }

                            let mut data = json!({
                                "text": text,
                                "index": next_send_index,
                            });
                            if let Some(spk) = res.speaker {
                                data["speaker"] = json!(spk);
                            }
                            let out = json!({
                                "event": "whisper:done",
                                "data": data,
                            }).to_string();
                            if tx.send(Message::Text(out)).await.is_err() {
                                pcm.clear();
                                return;
                            }
                        }
                        Err(e) => {
                            let out = json!({
                                "event": "whisper:error",
                                "data": e
                            }).to_string();
                            let _ = tx.send(Message::Text(out)).await;
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
                }
            }

            // ── Server shutdown ───────────────────────────────────────────────
            _ = shutdown_rx.recv() => {
                pcm.clear();
                let _ = tx.send(Message::Close(None)).await;
                break;
            }
        }
    }
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
