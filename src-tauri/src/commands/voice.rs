use crate::{db::queries, error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Debug, Serialize, Deserialize)]
pub struct TranscribeResult {
    pub text: String,
}

// ─── Server config ────────────────────────────────────────────────────────────

/// 從 DB 讀取 whisper-server 路徑、模型路徑、埠號
async fn resolve_whisper_server_config(
    state: &AppState,
) -> Result<(PathBuf, String, u16), AppError> {
    let pool = &state.db;

    let server_path = queries::get_setting(pool, "whisper_cli_path")
        .await?
        .unwrap_or_default();
    let model_path = queries::get_setting(pool, "whisper_model_path")
        .await?
        .unwrap_or_default();
    let port = queries::get_setting(pool, "whisper_server_port")
        .await?
        .unwrap_or_else(|| "8081".to_string())
        .parse::<u16>()
        .unwrap_or(8081);

    if server_path.is_empty() {
        return Err(AppError::Voice(
            "尚未設定 whisper-server 路徑，請到 Settings > Voice 設定。".to_string(),
        ));
    }
    if model_path.is_empty() {
        return Err(AppError::Voice(
            "尚未設定 Whisper 模型路徑，請到 Settings > Voice 設定。".to_string(),
        ));
    }

    let bin = PathBuf::from(&server_path);
    if !bin.exists() {
        return Err(AppError::Voice(format!(
            "找不到 whisper-server：{}",
            bin.display()
        )));
    }

    Ok((bin, model_path, port))
}

// ─── Server lifecycle ─────────────────────────────────────────────────────────

/// 確保 whisper-server 正在運行；若未啟動則自動 spawn
/// 回傳 base URL（例如 "http://127.0.0.1:8081"）
async fn ensure_whisper_server_running(
    state: &AppState,
    app: &AppHandle,
) -> Result<String, AppError> {
    let (bin, model_path, port) = resolve_whisper_server_config(state).await?;
    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::new();

    // 若 state 有子進程，先 ping 確認還活著
    // 任何 HTTP 回應（包含 404）都視為活著；只有連線被拒才代表進程已死
    {
        let guard = state.whisper_server.lock().await;
        if guard.is_some() {
            let alive = client
                .get(format!("{}/health", base_url))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok(); // 任何 HTTP 回應 = 伺服器仍在運行
            if alive {
                return Ok(base_url);
            }
            let _ = app.emit("whisper:stderr", "[server] 伺服器意外退出，重新啟動…");
        }
    }

    // 啟動 whisper-server
    let _ = app.emit(
        "whisper:stderr",
        &format!("[server] 啟動 whisper-server（port {}）…", port),
    );

    let mut child = tokio::process::Command::new(&bin)
        .args([
            "--model", &model_path,
            "--port", &port.to_string(),
            "--host", "127.0.0.1",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| AppError::Voice(format!("無法啟動 whisper-server：{}", e)))?;

    // 轉發 stderr 到 whisper:stderr 事件
    if let Some(stderr) = child.stderr.take() {
        let app2 = app.clone();
        tauri::async_runtime::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = app2.emit("whisper:stderr", &line);
            }
        });
    }

    *state.whisper_server.lock().await = Some(child);

    // 等待伺服器接受連線（最多 60s）
    // 任何 HTTP 回應（包含 404）代表進程已啟動；connection refused 代表尚未就緒
    let _ = app.emit("whisper:stderr", "[server] 等待模型載入…");
    for i in 0..60 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let result = client
            .get(format!("{}/health", base_url))
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        let ready = match result {
            Ok(_) => true,                           // 任何 HTTP 回應 = 就緒
            Err(ref e) if e.is_connect() => false,   // 連線被拒 = 尚未啟動
            Err(ref e) if e.is_timeout() => false,   // 逾時 = 尚未啟動
            Err(_) => true,                          // 其他 HTTP 層錯誤 = 伺服器存在
        };
        if ready {
            let _ = app.emit(
                "whisper:stderr",
                &format!("[server] 就緒（{}秒後）", i + 1),
            );
            return Ok(base_url);
        }
    }

    Err(AppError::Voice(
        "whisper-server 啟動逾時（60s），請確認路徑與模型設定。".to_string(),
    ))
}

// ─── Commands ─────────────────────────────────────────────────────────────────

/// 接收前端傳來的 PCM f32 音訊資料，透過 whisper-server HTTP API 進行轉錄
#[tauri::command]
pub async fn transcribe_audio(
    app: AppHandle,
    state: State<'_, AppState>,
    pcm_data: Vec<f32>,
    sample_rate: u32,
) -> Result<TranscribeResult, AppError> {
    let pool = &state.db;

    let model_path = queries::get_setting(pool, "whisper_model_path")
        .await?
        .unwrap_or_default();
    if model_path.is_empty() {
        return Err(AppError::Voice(
            "尚未設定 Whisper 模型路徑，請到 Settings > Voice 設定。".to_string(),
        ));
    }

    let lang = queries::get_setting(pool, "whisper_language")
        .await?
        .unwrap_or_else(|| "auto".to_string());

    // 寫入暫存 WAV 檔
    let temp_dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| AppError::Voice(e.to_string()))?;
    tokio::fs::create_dir_all(&temp_dir).await?;
    let wav_path = temp_dir.join("recording.wav");
    write_wav(&wav_path, &pcm_data, sample_rate, 1)?;

    // 確保 whisper-server 正在運行
    let base_url = ensure_whisper_server_running(&state, &app).await?;

    // 讀取 WAV bytes
    let file_bytes = tokio::fs::read(&wav_path)
        .await
        .map_err(|e| AppError::Voice(format!("讀取 WAV 失敗：{}", e)))?;
    tokio::fs::remove_file(&wav_path).await.ok();

    // POST multipart 到 whisper-server /inference
    let client = reqwest::Client::new();
    let part = reqwest::multipart::Part::bytes(file_bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| AppError::Voice(e.to_string()))?;
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("response_format", "json")
        .text("language", lang)
        .text("temperature", "0.0");

    let resp = client
        .post(format!("{}/inference", base_url))
        .multipart(form)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| AppError::Voice(format!("whisper-server 請求失敗：{}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Voice(format!(
            "whisper-server 回傳錯誤 {}：{}",
            status, body
        )));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::Voice(format!("解析回應失敗：{}", e)))?;

    let text = json["text"].as_str().unwrap_or("").trim().to_string();
    Ok(TranscribeResult { text })
}

/// App 啟動時呼叫：若已設定路徑則背景預熱 whisper-server 並送出靜音推論
/// 不阻塞啟動流程；錯誤直接忽略（使用者未設定時是正常情況）
pub async fn warmup_whisper_server(state: &AppState, app: &AppHandle) {
    let configured = matches!(
        queries::get_setting(&state.db, "whisper_cli_path").await,
        Ok(Some(ref p)) if !p.is_empty()
    );
    if configured {
        if let Ok(base_url) = ensure_whisper_server_running(state, app).await {
            // 送一段靜音給 whisper 做推論預熱，觸發 Metal shader 編譯 / 內部緩衝區分配
            // 這樣使用者第一次錄音時推論速度就會與後續段落相同
            let _ = warmup_whisper_inference(&base_url).await;
            let _ = app.emit("whisper:stderr", "[server] 推論引擎預熱完成");
        }
    }
}

/// 送出 1 秒靜音 WAV 到 /inference，觸發 whisper 內部的首次推論初始化
async fn warmup_whisper_inference(base_url: &str) {
    let wav_bytes = build_silent_wav(1.0, 16000);
    let client = reqwest::Client::new();
    let Ok(part) = reqwest::multipart::Part::bytes(wav_bytes)
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
    // 結果不重要（靜音的辨識結果為空），只是為了讓推論引擎完成初始化
    let _ = client
        .post(format!("{}/inference", base_url))
        .multipart(form)
        .timeout(Duration::from_secs(60))
        .send()
        .await;
}

/// 建立指定秒數的靜音 WAV（單聲道 16-bit PCM）
fn build_silent_wav(duration_secs: f32, sample_rate: u32) -> Vec<u8> {
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
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    buf.extend_from_slice(&vec![0u8; data_size as usize]); // 靜音
    buf
}

/// 手動停止 whisper-server（App 關閉時也會自動呼叫）
#[tauri::command]
pub async fn stop_whisper_server(state: State<'_, AppState>) -> Result<(), AppError> {
    let mut guard = state.whisper_server.lock().await;
    if let Some(mut child) = guard.take() {
        child.kill().await.ok();
    }
    Ok(())
}

// ─── WAV helper ───────────────────────────────────────────────────────────────

fn write_wav(path: &PathBuf, pcm: &[f32], sample_rate: u32, channels: u16) -> Result<(), AppError> {
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align = channels * bits_per_sample / 8;
    let data_size = (pcm.len() * 2) as u32;
    let file_size = 36 + data_size;

    let mut buf = Vec::with_capacity(44 + pcm.len() * 2);

    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");

    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());

    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());

    for &sample in pcm {
        let clamped = sample.clamp(-1.0, 1.0);
        let i16_sample = (clamped * 32767.0) as i16;
        buf.extend_from_slice(&i16_sample.to_le_bytes());
    }

    std::fs::write(path, buf).map_err(|e| AppError::Voice(e.to_string()))?;
    Ok(())
}
