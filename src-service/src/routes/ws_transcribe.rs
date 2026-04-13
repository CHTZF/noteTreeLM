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

/// Result of one transcription job, forwarded back to the sender loop.
struct TranscribeResult {
    index: u32,
    outcome: Result<String, String>,
}

/// Spawn a transcription task. Results are sent through `result_tx`.
/// `shutdown_tx` is subscribed to cancel the task if server shuts down.
fn spawn_transcribe(
    state: ApiState,
    samples: Vec<i16>,
    language: String,
    index: u32,
    result_tx: mpsc::Sender<TranscribeResult>,
) {
    let mut shutdown_rx = state.daemon.ws_shutdown_tx.subscribe();
    tokio::spawn(async move {
        let outcome = tokio::select! {
            r = crate::routes::whisper::transcribe_pcm16(&state, &samples, &language) => r,
            _ = shutdown_rx.recv() => Err("server shutdown".to_string()),
        };
        // Ignore send error — WS handler may have already exited
        let _ = result_tx.send(TranscribeResult { index, outcome }).await;
    });
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
    // Capacity 16: allows up to 16 in-flight segments before back-pressure
    let (result_tx, mut result_rx) = mpsc::channel::<TranscribeResult>(16);

    // Track pending tasks so stop() can wait for them all
    let mut pending_tasks: u32 = 0;
    // next_send_index: the segment index we're waiting to forward next (for ordered output)
    let mut next_send_index: u32 = 0;
    // Out-of-order result buffer: index → text
    let mut result_buf: std::collections::HashMap<u32, Result<String, String>> = std::collections::HashMap::new();

    // ─── Per-connection state ──────────────────────────────────────────────────
    let mut language        = "auto".to_string();
    let mut pcm: Vec<i16>  = Vec::new();
    let mut speech_active   = false;
    let mut silence_samples : usize = 0;
    let mut chunk_start     : usize = 0;
    let mut segment_index   : u32   = 0;
    let mut active          = false;
    // When true, we've sent "stop" and are draining pending tasks
    let mut stopping        = false;

    loop {
        tokio::select! {
            // ── Incoming WS message ───────────────────────────────────────────
            msg_opt = rx.next() => {
                let msg = match msg_opt {
                    Some(Ok(m)) => m,
                    _ => {
                        // Client disconnected — clear buffer, background tasks will
                        // drain naturally (result_tx dropped when result_rx drops)
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
                                active          = true;
                                stopping        = false;
                            }

                            Some("stop") => {
                                if active {
                                    // Flush remaining buffer as final segment
                                    let remaining = pcm[chunk_start..].to_vec();
                                    if remaining.len() >= MIN_SEGMENT_SAMPLES
                                        && rms_i16(&remaining) >= MIN_CHUNK_RMS
                                    {
                                        spawn_transcribe(
                                            state.clone(), remaining, language.clone(),
                                            segment_index, result_tx.clone(),
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

                                    // If nothing pending, flush_done immediately
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
                                // Drain any pending results silently
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
                                    spawn_transcribe(
                                        state.clone(), chunk, language.clone(),
                                        segment_index, result_tx.clone(),
                                    );
                                    segment_index += 1;
                                    pending_tasks += 1;
                                }
                                chunk_start   = pcm.len();
                                speech_active = false;
                                silence_samples = 0;

                                // Compact
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
                                    spawn_transcribe(
                                        state.clone(), chunk.to_vec(), language.clone(),
                                        segment_index, result_tx.clone(),
                                    );
                                    segment_index += 1;
                                    pending_tasks += 1;
                                }

                                chunk_start = end_idx;

                                // Compact processed audio (keep at most 60s of raw)
                                if chunk_start > SAMPLE_RATE * 60 {
                                    pcm.drain(0..chunk_start);
                                    chunk_start = 0;
                                }
                            } else if !speech_active
                                && (pcm.len() - chunk_start) > AMBIENT_DRAIN_SAMPLES
                            {
                                // Long ambient silence — discard to prevent unbounded growth
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
                result_buf.insert(res.index, res.outcome);
                while let Some(outcome) = result_buf.remove(&next_send_index) {
                    match outcome {
                        Ok(text) if !text.is_empty() => {
                            let out = json!({
                                "event": "whisper:done",
                                "data": { "text": text, "index": next_send_index }
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

                // If we were waiting to stop and all tasks are done → flush_done
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
