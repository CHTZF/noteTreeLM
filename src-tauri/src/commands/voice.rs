use crate::{error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tauri::{AppHandle, State};

#[derive(Debug, Serialize, Deserialize)]
pub struct TranscribeResult {
    pub text: String,
}

// ─── Thin wrappers to notetreelm-service /api/v1/whisper/* ───────────────────

/// Tauri command: 接收 PCM，建構 WAV，委派給 service 轉錄
#[tauri::command]
pub async fn transcribe_audio(
    _app: AppHandle,
    state: State<'_, AppState>,
    pcm_data: Vec<f32>,
    sample_rate: u32,
    language: Option<String>,
) -> Result<TranscribeResult, AppError> {
    let wav = build_wav_bytes(&pcm_data, sample_rate);
    let lang = language.unwrap_or_else(|| "auto".to_string());

    let client = reqwest::Client::new();
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| AppError::Voice(e.to_string()))?;

    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("language", lang);

    let tok_owned = state.get_auth_token().await;
    let mut req = client
        .post("http://127.0.0.1:7787/api/v1/whisper/transcribe")
        .multipart(form)
        .timeout(Duration::from_secs(120));
    if !tok_owned.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", tok_owned));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| AppError::Voice(format!("service 請求失敗：{}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::Voice(format!("轉錄失敗 {}：{}", status, body)));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::Voice(format!("解析回應失敗：{}", e)))?;

    let text = json["text"].as_str().unwrap_or("").to_string();
    Ok(TranscribeResult { text })
}

/// App 啟動時呼叫：背景觸發 service 啟動並預熱 whisper-server
pub async fn warmup_whisper_server(state: &AppState, _app: &AppHandle) {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    // Best-effort; 未設定路徑時 service 會靜默略過
    let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/whisper/start",
        &serde_json::json!({}),
        tok,
    ).await;
}

#[tauri::command]
pub async fn get_whisper_server_status(state: State<'_, AppState>) -> Result<String, AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    let val: serde_json::Value = crate::api_client::daemon_get(
        &state.http_client,
        "/whisper/status",
        tok,
    ).await.unwrap_or_else(|_| serde_json::json!({ "status": "stopped" }));
    Ok(val["status"].as_str().unwrap_or("stopped").to_string())
}

#[tauri::command]
pub async fn start_whisper_server(
    _app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/whisper/start",
        &serde_json::json!({}),
        tok,
    ).await.map_err(|e| AppError::Voice(e.to_string()))?;
    Ok(())
}

#[tauri::command]
pub async fn stop_whisper_server(state: State<'_, AppState>) -> Result<(), AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/whisper/stop",
        &serde_json::json!({}),
        tok,
    ).await.map_err(|e| AppError::Voice(e.to_string()))?;
    Ok(())
}

#[tauri::command]
pub async fn restart_whisper_server(
    _app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok = if tok_owned.is_empty() { None } else { Some(tok_owned.as_str()) };
    crate::api_client::daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/whisper/restart",
        &serde_json::json!({}),
        tok,
    ).await.map_err(|e| AppError::Voice(e.to_string()))?;
    Ok(())
}

// ─── WAV builder (pure format conversion, stays in Tauri) ────────────────────

/// 將 PCM f32 建構成單聲道 16-bit WAV bytes（純記憶體，無磁碟 I/O）
fn build_wav_bytes(pcm: &[f32], sample_rate: u32) -> Vec<u8> {
    let channels: u16 = 1;
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
    buf.extend_from_slice(&1u16.to_le_bytes());
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
    buf
}
