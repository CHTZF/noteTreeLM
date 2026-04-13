use axum::{
    extract::{Multipart, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::atomic::Ordering, time::Duration};

use crate::app_state::ApiState;
use crate::db::SurrealDb;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/whisper/status", get(status_handler))
        .route("/whisper/start", post(start_handler))
        .route("/whisper/stop", post(stop_handler))
        .route("/whisper/restart", post(restart_handler))
        .route("/whisper/transcribe", post(transcribe_handler))
}

// ─── Settings ─────────────────────────────────────────────────────────────────

async fn get_setting(db: &SurrealDb, key: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Row { value: String }
    db.query("SELECT `value` FROM `settings` WHERE `key` = $key LIMIT 1")
        .bind(("key", key.to_string()))
        .await
        .ok()?
        .take::<Vec<Row>>(0)
        .ok()?
        .into_iter()
        .next()
        .map(|r| r.value)
        .filter(|v| !v.is_empty())
}


struct WhisperConfig {
    bin: PathBuf,
    model_path: String,
    threads: u32,
}

async fn resolve_config(db: &SurrealDb) -> Result<WhisperConfig, String> {
    let cli_path = get_setting(db, "whisper_cli_path").await.unwrap_or_default();
    let cli_path = cli_path.trim().trim_matches('"').trim_matches('\'').to_string();
    let model_path = get_setting(db, "whisper_model_path").await.unwrap_or_default();
    let model_path = model_path.trim().trim_matches('"').trim_matches('\'').to_string();
    let threads: u32 = get_setting(db, "whisper_threads").await
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);

    if cli_path.is_empty() {
        return Err("尚未設定 whisper-server 路徑，請到 Settings > Voice 設定。".to_string());
    }
    if model_path.is_empty() {
        return Err("尚未設定 Whisper 模型路徑，請到 Settings > Voice 設定。".to_string());
    }

    #[cfg(not(windows))]
    let bin = PathBuf::from(&cli_path);
    #[cfg(windows)]
    let bin = {
        let b = PathBuf::from(&cli_path);
        if !b.exists() && b.extension().is_none() {
            let candidate = b.with_extension("exe");
            if candidate.exists() { candidate } else { b }
        } else { b }
    };

    if !bin.exists() {
        return Err(format!(
            "找不到 whisper-server：{}，請到 Settings > Voice 更新路徑。",
            bin.display()
        ));
    }
    let model_file = PathBuf::from(&model_path);
    if !model_file.exists() {
        return Err(format!(
            "找不到 Whisper 模型檔案：{}，請到 Settings > Voice 更新路徑。",
            model_file.display()
        ));
    }

    Ok(WhisperConfig { bin, model_path, threads })
}

// ─── Server lifecycle ─────────────────────────────────────────────────────────

async fn ensure_running(state: &ApiState) -> Result<String, String> {
    if state.daemon.whisper_user_stopped.load(Ordering::SeqCst) {
        return Err("whisper-server 已手動停止".to_string());
    }

    let _lock = state.daemon.whisper_start_lock.lock().await;

    let config = resolve_config(&state.db).await?;

    let base_url = state.daemon.whisper_url.clone();
    let client = &state.daemon.http_client;

    enum Action { Spawn, WaitExisting, Ready }

    let action = {
        let mut guard = state.daemon.whisper_server.lock().await;
        match guard.as_mut() {
            None => {
                let orphan_alive = client
                    .get(format!("{}/health", base_url))
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                    .is_ok();
                if orphan_alive {
                    state.daemon.emit("whisper:stderr", json!("[server] 偵測到既有 whisper-server，重新使用"));
                    Action::Ready
                } else {
                    Action::Spawn
                }
            }
            Some(child) => {
                let alive = client
                    .get(format!("{}/health", base_url))
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                    .is_ok();
                if alive {
                    Action::Ready
                } else {
                    match child.try_wait() {
                        Ok(None) => Action::WaitExisting,
                        _ => {
                            state.daemon.emit("whisper:stderr", json!("[server] 伺服器意外退出，重新啟動…"));
                            *guard = None;
                            Action::Spawn
                        }
                    }
                }
            }
        }
    };

    match action {
        Action::Ready => return Ok(base_url),
        Action::WaitExisting => {
            state.daemon.emit("whisper:stderr", json!("[server] 模型載入中，請稍候…"));
        }
        Action::Spawn => {
            let port = state.daemon.whisper_url.rsplit(':').next()
                .and_then(|p| p.parse::<u16>().ok()).unwrap_or(18082);
            state.daemon.emit(
                "whisper:stderr",
                json!(format!("[server] 啟動 whisper-server（port {}）…", port)),
            );

            let mut cmd = tokio::process::Command::new(&config.bin);
            cmd.args([
                "--model", &config.model_path,
                "--port", &port.to_string(),
                "--host", "127.0.0.1",
                "--threads", &config.threads.to_string(),
            ]);
            #[cfg(target_os = "macos")]
            cmd.arg("--flash-attn");
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(false);
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000);
            }
            let mut child = cmd
                .spawn()
                .map_err(|e| format!("無法啟動 whisper-server：{}", e))?;

            // Forward stderr → SSE
            if let Some(stderr) = child.stderr.take() {
                let emitter = state.daemon.event_tx.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, BufReader};
                    let mut reader = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = reader.next_line().await {
                        let _ = emitter.send(crate::service::types::ServiceEvent {
                            event: "whisper:stderr".to_string(),
                            payload: json!(line),
                        });
                    }
                });
            }

            *state.daemon.whisper_server.lock().await = Some(child);
        }
    }

    // Wait for server ready (up to 180s)
    state.daemon.emit("whisper:stderr", json!("[server] 等待模型載入…"));
    for i in 0..180 {
        tokio::time::sleep(Duration::from_secs(1)).await;

        {
            let mut guard = state.daemon.whisper_server.lock().await;
            match guard.as_mut() {
                None => return Err("whisper-server 已手動停止".to_string()),
                Some(child) => {
                    if let Ok(Some(status)) = child.try_wait() {
                        *guard = None;
                        return Err(format!(
                            "whisper-server 意外退出（code: {:?}），請確認模型路徑與二進位設定。",
                            status.code()
                        ));
                    }
                }
            }
        }

        let result = client
            .get(format!("{}/health", base_url))
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        let ready = match result {
            Ok(_) => true,
            Err(ref e) if e.is_connect() => false,
            Err(ref e) if e.is_timeout() => false,
            Err(_) => true,
        };
        if ready {
            state.daemon.emit(
                "whisper:stderr",
                json!(format!("[server] 就緒（{}秒後）", i + 1)),
            );
            return Ok(base_url);
        }
    }

    Err("whisper-server 啟動逾時（180s），請確認路徑與模型設定。".to_string())
}

// ─── Language / prompt mapping ────────────────────────────────────────────────

const ZH_TW_PROMPT: &str = "以下是繁體中文語音辨識內容，使用臺灣標準繁體字。\
    這段語音辨識結果將以正體中文呈現，\
    包含學習、時間、語言、發展、國家、開始、對話、動態、實際、問題、\
    關係、經驗、處理、業務、義務、氣候、類別、讓步、點選、話語等詞彙，\
    確保所有輸出均為繁體字型。";

fn map_language(lang: &str) -> (&str, Option<&str>) {
    match lang {
        "zh-TW" | "zh-tw" => ("zh", Some(ZH_TW_PROMPT)),
        "zh-CN" | "zh-cn" | "zh" => ("zh", Some("以下是普通话语音识别内容，使用简体中文书写，包括学习、时间、语言、发展、国家、问题等词汇。")),
        other => (other, None),
    }
}

// ─── Hallucination filter ─────────────────────────────────────────────────────

fn is_hallucination(text: &str) -> bool {
    let s = text.trim().to_lowercase();
    let s = s.trim_end_matches(['.', '!', '?', ',', '。', '！', '？', '…']).trim();
    matches!(
        s,
        "thank you" | "thanks" | "thanks for watching" | "thank you for watching"
            | "thank you for listening" | "please subscribe" | "like and subscribe"
            | "subscribe" | "bye" | "bye bye" | "you"
            | "subtitles by the amara.org community"
            | "[silence]" | "[ silence ]" | "[blank_audio]" | "[ blank_audio ]"
            | "[music]" | "[ music ]" | "[applause]"
            | "[音乐]" | "[ 音乐 ]" | "[掌声]" | "[ 掌声 ]"
            | "[音樂]" | "[ 音樂 ]" | "[掌聲]" | "[ 掌聲 ]"
            | "謝謝" | "謝謝你" | "謝謝大家" | "謝謝觀看" | "謝謝收看"
            | "訂閱" | "請訂閱" | "按讚訂閱"
            | "谢谢" | "谢谢你" | "谢谢大家" | "谢谢观看" | "谢谢收看"
            | "订阅" | "请订阅" | "请关注"
    )
}

// ─── WAV helpers ──────────────────────────────────────────────────────────────

/// Build a WAV file from raw i16 PCM samples (mono, little-endian).
pub fn build_wav_from_i16(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let n_samples = samples.len() as u32;
    let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align = channels * bits_per_sample / 8;
    let data_size = n_samples * 2;
    let file_size = 36 + data_size;

    let mut buf = Vec::with_capacity(44 + data_size as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    for &s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    buf
}

/// Transcribe raw PCM16 (16-bit signed LE, 16 kHz mono) via whisper-server.
/// Returns the recognized text (empty string if hallucination/silence), or an error.
pub async fn transcribe_pcm16(
    state: &ApiState,
    samples: &[i16],
    language: &str,
) -> Result<String, String> {
    let wav = build_wav_from_i16(samples, 16000);
    let base_url = ensure_running(state).await?;
    let (whisper_lang, initial_prompt) = map_language(language);

    let client = &state.daemon.http_client;
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("response_format", "json")
        .text("language", whisper_lang.to_string())
        .text("temperature", "0.0")
        .text("beam_size", "3")
        .text("no_speech_thold", "0.6")
        .text("suppress_blank", "true");

    if let Some(prompt) = initial_prompt {
        form = form.text("initial_prompt", prompt.to_string());
    }

    let resp = client
        .post(format!("{}/inference", base_url))
        .multipart(form)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("whisper-server 請求失敗：{}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("whisper-server 回傳錯誤 {}：{}", status, body));
    }

    let json: serde_json::Value = resp.json().await
        .map_err(|e| format!("解析回應失敗：{}", e))?;

    let text = json["text"].as_str().unwrap_or("").trim().to_string();
    let text = if is_hallucination(&text) { String::new() } else { text };
    Ok(text)
}

pub fn build_silent_wav(duration_secs: f32, sample_rate: u32) -> Vec<u8> {
    let n_samples = (duration_secs * sample_rate as f32) as u32;
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align = channels * bits_per_sample / 8;
    let data_size = n_samples * 2;
    let file_size = 36 + data_size;

    let mut buf = Vec::with_capacity(44 + data_size as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    buf.extend_from_slice(&vec![0u8; data_size as usize]);
    buf
}

async fn warmup_inference(base_url: &str, client: &reqwest::Client) {
    let wav = build_silent_wav(1.0, 16000);
    let Ok(part) = reqwest::multipart::Part::bytes(wav)
        .file_name("warmup.wav")
        .mime_str("audio/wav")
    else {
        return;
    };
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("response_format", "json")
        .text("language", "auto")
        .text("temperature", "0.0");
    let _ = client
        .post(format!("{}/inference", base_url))
        .multipart(form)
        .timeout(Duration::from_secs(60))
        .send()
        .await;
}

// ─── Route handlers ───────────────────────────────────────────────────────────

async fn transcribe_handler(
    State(state): State<ApiState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut wav_bytes: Option<Vec<u8>> = None;
    let mut language = "auto".to_string();

    while let Some(field) = multipart.next_field().await
        .map_err(|e: axum::extract::multipart::MultipartError| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                let bytes = field.bytes().await
                    .map_err(|e: axum::extract::multipart::MultipartError| (StatusCode::BAD_REQUEST, e.to_string()))?;
                wav_bytes = Some(bytes.to_vec());
            }
            "language" => {
                let text = field.bytes().await
                    .map_err(|e: axum::extract::multipart::MultipartError| (StatusCode::BAD_REQUEST, e.to_string()))?;
                language = String::from_utf8_lossy(&text).into_owned();
            }
            _ => {
                // drain unknown fields
                let _ = field.bytes().await;
            }
        }
    }

    let wav = wav_bytes.ok_or((StatusCode::BAD_REQUEST, "missing audio file".to_string()))?;
    let (whisper_lang, initial_prompt) = map_language(&language);

    let base_url = ensure_running(&state).await.map_err(|e| {
        (StatusCode::SERVICE_UNAVAILABLE, e)
    })?;

    let client = &state.daemon.http_client;
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("response_format", "json")
        .text("language", whisper_lang.to_string())
        .text("temperature", "0.0")
        .text("beam_size", "3")
        .text("no_speech_thold", "0.6")
        .text("suppress_blank", "true");

    if let Some(prompt) = initial_prompt {
        form = form.text("initial_prompt", prompt.to_string());
    }

    let resp = client
        .post(format!("{}/inference", base_url))
        .multipart(form)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("whisper-server 請求失敗：{}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err((StatusCode::BAD_GATEWAY, format!("whisper-server 回傳錯誤 {}：{}", status, body)));
    }

    let json: Value = resp.json().await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("解析回應失敗：{}", e)))?;

    let text = json["text"].as_str().unwrap_or("").trim().to_string();
    let text = if is_hallucination(&text) { String::new() } else { text };

    Ok(Json(json!({ "text": text })))
}

async fn status_handler(
    State(state): State<ApiState>,
) -> Json<Value> {
    let healthy = state.daemon.http_client
        .get(format!("{}/health", state.daemon.whisper_url))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .is_ok();

    if healthy {
        return Json(json!({ "status": "running" }));
    }

    let mut guard = state.daemon.whisper_server.lock().await;
    match guard.as_mut() {
        None => Json(json!({ "status": "stopped" })),
        Some(child) => match child.try_wait() {
            Ok(None) => Json(json!({ "status": "loading" })),
            _ => {
                *guard = None;
                Json(json!({ "status": "stopped" }))
            }
        },
    }
}

async fn start_handler(
    State(state): State<ApiState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state.daemon.whisper_user_stopped.store(false, Ordering::SeqCst);

    // Best-effort: check settings configured first
    let cli_configured = get_setting(&state.db, "whisper_cli_path").await.is_some();
    let model_configured = get_setting(&state.db, "whisper_model_path").await.is_some();
    if !cli_configured || !model_configured {
        return Ok(Json(json!({ "ok": true, "skipped": "not_configured" })));
    }

    let base_url = ensure_running(&state).await.map_err(|e| {
        state.daemon.emit("whisper:stderr", json!(format!("[server:error] {}", e)));
        (StatusCode::INTERNAL_SERVER_ERROR, e)
    })?;
    warmup_inference(&base_url, &state.daemon.http_client).await;
    state.daemon.emit("whisper:stderr", json!("[server] 推論引擎預熱完成"));

    Ok(Json(json!({ "ok": true })))
}

async fn stop_handler(
    State(state): State<ApiState>,
) -> Json<Value> {
    state.daemon.whisper_user_stopped.store(true, Ordering::SeqCst);

    let mut guard = state.daemon.whisper_server.lock().await;
    if let Some(mut child) = guard.take() {
        child.kill().await.ok();
        child.wait().await.ok();
    }

    Json(json!({ "ok": true }))
}

async fn restart_handler(
    State(state): State<ApiState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state.daemon.whisper_user_stopped.store(false, Ordering::SeqCst);

    {
        let mut guard = state.daemon.whisper_server.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill().await.ok();
            child.wait().await.ok();
        }
    }

    let base_url = ensure_running(&state).await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, e)
    })?;
    warmup_inference(&base_url, &state.daemon.http_client).await;

    Ok(Json(json!({ "ok": true })))
}

/// Called on service shutdown — kill whisper process gracefully.
pub async fn kill_on_shutdown(state: &crate::daemon::state::DaemonState) {
    let mut guard = state.whisper_server.lock().await;
    if let Some(mut child) = guard.take() {
        child.kill().await.ok();
        child.wait().await.ok();
    }
}
