use crate::{api_client::daemon_get_setting, error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};

#[derive(Debug, Serialize, Deserialize)]
pub struct TranscribeResult {
    pub text: String,
}

// ─── Server config ────────────────────────────────────────────────────────────

/// 從可用 port 開始往上尋找第一個空閒的 localhost port
fn find_free_port(preferred: u16) -> u16 {
    use std::net::TcpListener;
    for port in preferred..=65535 {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    preferred
}

/// 從 daemon 讀取 whisper-server 路徑、模型路徑、threads（port 由執行時自動分配）
async fn resolve_whisper_server_config(
    state: &AppState,
) -> Result<(PathBuf, String, u32), AppError> {
    let tok_owned = state.get_auth_token().await;
    let tok: Option<&str> = if tok_owned.is_empty() { None } else { Some(&tok_owned) };

    let server_path = daemon_get_setting(&state.http_client, tok, "whisper_cli_path")
        .await
        .unwrap_or_default();
    // Trim whitespace and surrounding quotes (users sometimes paste quoted paths from terminal)
    let server_path = server_path.trim().trim_matches('"').trim_matches('\'').to_string();

    let model_path = daemon_get_setting(&state.http_client, tok, "whisper_model_path")
        .await
        .unwrap_or_default();
    let model_path = model_path.trim().trim_matches('"').trim_matches('\'').to_string();

    let threads: u32 = daemon_get_setting(&state.http_client, tok, "whisper_threads")
        .await
        .unwrap_or_default()
        .parse()
        .unwrap_or(4);

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

    #[cfg(not(windows))]
    let bin = PathBuf::from(&server_path);
    // On Windows, auto-append .exe if the path has no extension and the bare path doesn't exist.
    #[cfg(windows)]
    let bin = {
        let b = PathBuf::from(&server_path);
        if !b.exists() && b.extension().is_none() {
            let candidate = b.with_extension("exe");
            if candidate.exists() { candidate } else { b }
        } else { b }
    };
    if !bin.exists() {
        return Err(AppError::Voice(format!(
            "找不到 whisper-server：{}，請到 Settings > Voice 更新路徑。",
            bin.display()
        )));
    }

    // 在嘗試啟動伺服器前先確認模型檔案存在，避免 server spawn 後立刻因載入失敗退出
    let model_file = PathBuf::from(&model_path);
    if !model_file.exists() {
        return Err(AppError::Voice(format!(
            "找不到 Whisper 模型檔案：{}，請到 Settings > Voice 更新路徑。",
            model_file.display()
        )));
    }

    Ok((bin, model_path, threads))
}

// ─── Server lifecycle ─────────────────────────────────────────────────────────

/// 確保 whisper-server 正在運行；若未啟動則自動 spawn
/// 回傳 base URL（例如 "http://127.0.0.1:8081"）
async fn ensure_whisper_server_running(
    state: &AppState,
    app: &AppHandle,
) -> Result<String, AppError> {
    // 使用者主動停止旗標：若為 true，拒絕自動重啟（防止 transcribe_audio 在停止後重啟）
    if state.whisper_user_stopped.load(Ordering::SeqCst) {
        return Err(AppError::Voice("whisper-server 已手動停止".to_string()));
    }

    // 啟動鎖：確保同一時刻只有一個呼叫者在跑啟動 / 等待流程
    // 第二個呼叫者在此等待，直到第一個完成後再進入 Phase 1
    // → 第二個呼叫者在 Phase 1 發現伺服器已就緒，直接回傳，不重複 emit
    let _start_lock = state.whisper_start_lock.lock().await;

    let (bin, model_path, threads) = resolve_whisper_server_config(state).await?;

    // 取得或自動分配 port（只分配一次，後續重用）
    // port_allocator 確保 whisper 與 llama 不會並發 find_free_port 取到同一 port
    let port = {
        let _alloc_lock = state.port_allocator.lock().await;
        let mut guard = state.whisper_actual_port.lock().await;
        if let Some(p) = *guard {
            p
        } else {
            let p = find_free_port(8081);
            *guard = Some(p);
            p
        }
    };

    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::new();

    // ── Phase 1：判斷是否需要 spawn ──────────────────────────────────────
    // 三種情況：
    //   A) 無子進程 → 直接 spawn
    //   B) 有子進程且 health OK → 直接回傳
    //   C) 有子進程且 health 失敗 → 檢查進程是否仍在執行
    //      C1) 仍在執行（只是還在載入） → 跳過 spawn，直接進入等待迴圈
    //      C2) 已結束（crash） → 清除 state，重新 spawn
    enum Action { Spawn, WaitExisting, Ready }

    let action = {
        let mut guard = state.whisper_server.lock().await;
        match guard.as_mut() {
            None => {
                // 無子進程記錄，但 port 上可能有孤立進程（上次 crash / force-quit 殘留）
                // 先快速 ping 一次：若已有回應就直接使用，避免重複 spawn 造成 port 衝突
                let orphan_alive = client
                    .get(format!("{}/health", base_url))
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                    .is_ok();
                if orphan_alive {
                    let _ = app.emit("whisper:stderr", "[server] 偵測到既有 whisper-server，重新使用");
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
                    // try_wait() 不阻塞：Ok(None) = 仍在執行，Ok(Some(_)) = 已結束
                    match child.try_wait() {
                        Ok(None) => {
                            // 進程仍在，只是模型尚未載入完畢（warmup 逾時後常見）
                            Action::WaitExisting
                        }
                        _ => {
                            // 進程已結束或狀態無法取得 → 清除並重新 spawn
                            let _ = app.emit("whisper:stderr", "[server] 伺服器意外退出，重新啟動…");
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
            let _ = app.emit("whisper:stderr", "[server] 模型載入中，請稍候…");
            // 直接進入等待迴圈，不重新 spawn
        }
        Action::Spawn => {
            let _ = app.emit(
                "whisper:stderr",
                &format!("[server] 啟動 whisper-server（port {}）…", port),
            );

            let mut cmd = tokio::process::Command::new(&bin);
            cmd.args([
                "--model", &model_path,
                "--port", &port.to_string(),
                "--host", "127.0.0.1",
                "--threads", &threads.to_string(),
            ]);
            // --flash-attn is a Metal-only flag; passing it on Windows causes the server to exit immediately.
            #[cfg(target_os = "macos")]
            cmd.arg("--flash-attn");
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(false);
            // Windows: CREATE_NO_WINDOW (0x08000000) — prevent console window from appearing
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000);
            }
            let mut child = cmd
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
        }
    }

    // ── Phase 2：等待伺服器就緒（最多 180s，支援大型模型）────────────────
    // 每秒：先確認進程是否仍在執行，再 ping health endpoint
    // 若進程提早退出（模型路徑錯誤等）立即回報錯誤，不等滿 180s
    let _ = app.emit("whisper:stderr", "[server] 等待模型載入…");
    for i in 0..180 {
        tokio::time::sleep(Duration::from_secs(1)).await;

        // 先檢查進程是否已退出或被手動停止
        {
            let mut guard = state.whisper_server.lock().await;
            match guard.as_mut() {
                None => {
                    // state 被 stop_whisper_server 清空 → 使用者手動停止，立即放棄
                    return Err(AppError::Voice(
                        "whisper-server 已手動停止".to_string(),
                    ));
                }
                Some(child) => {
                    if let Ok(Some(status)) = child.try_wait() {
                        // 進程已退出（模型路徑錯誤、二進位不相容等）
                        *guard = None;
                        return Err(AppError::Voice(format!(
                            "whisper-server 意外退出（code: {:?}），請確認模型路徑與二進位設定。",
                            status.code()
                        )));
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
        "whisper-server 啟動逾時（180s），請確認路徑與模型設定。".to_string(),
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
    let tok_owned = state.get_auth_token().await;
    let tok: Option<&str> = if tok_owned.is_empty() { None } else { Some(&tok_owned) };

    let model_path = daemon_get_setting(&state.http_client, tok, "whisper_model_path")
        .await
        .unwrap_or_default();
    if model_path.is_empty() {
        return Err(AppError::Voice(
            "尚未設定 Whisper 模型路徑，請到 Settings > Voice 設定。".to_string(),
        ));
    }

    let lang = daemon_get_setting(&state.http_client, tok, "whisper_language")
        .await
        .unwrap_or_else(|| "auto".to_string());

    // zh-TW / zh-CN 都使用 whisper 的 "zh" 語言代碼，
    // 但加入 initial_prompt 導引輸出字體（whisper 無法區分繁簡，靠 prompt 引導）
    // zh-TW 使用較長的繁體中文 prompt，讓 whisper decoder 強烈偏向繁體字
    // （單句短 prompt 不足以覆蓋模型對簡體的訓練偏好）
    const ZH_TW_PROMPT: &str = "以下是繁體中文語音辨識內容，使用臺灣標準繁體字。\
        這段語音辨識結果將以正體中文呈現，\
        包含學習、時間、語言、發展、國家、開始、對話、動態、實際、問題、\
        關係、經驗、處理、業務、義務、氣候、類別、讓步、點選、話語等詞彙，\
        確保所有輸出均為繁體字型。";
    let (whisper_lang, initial_prompt): (&str, Option<&str>) = match lang.as_str() {
        "zh-TW" => ("zh", Some(ZH_TW_PROMPT)),
        "zh-CN" => ("zh", Some("以下是普通话语音识别内容，使用简体中文书写，包括学习、时间、语言、发展、国家、问题等词汇。")),
        other   => (other, None),
    };

    // 在記憶體中建構 WAV，避免磁碟 I/O 造成延遲
    let wav_bytes = build_wav_bytes(&pcm_data, sample_rate);

    // 確保 whisper-server 正在運行
    let base_url = ensure_whisper_server_running(&state, &app).await?;

    // POST multipart 到 whisper-server /inference
    let client = reqwest::Client::new();
    let part = reqwest::multipart::Part::bytes(wav_bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| AppError::Voice(e.to_string()))?;
    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("response_format", "json")
        .text("language", whisper_lang.to_string())
        .text("temperature", "0.0")
        .text("beam_size", "3")           // beam=3：準確度提升明顯，延遲增加有限
        .text("no_speech_thold", "0.6")   // 無語音概率 > 0.6 → 輸出空字串，抑制幻覺
        .text("suppress_blank", "true");  // 抑制空白 token，進一步減少雜訊幻覺
    if let Some(prompt) = initial_prompt {
        form = form.text("initial_prompt", prompt.to_string());
    }

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
    let text = if is_whisper_hallucination(&text) { String::new() } else { text };
    Ok(TranscribeResult { text })
}

/// Whisper 在靜音或雜訊片段上的已知幻覺短句過濾
/// temperature=0 仍會出現這些輸出，需在應用層過濾
fn is_whisper_hallucination(text: &str) -> bool {
    let s = text.trim().to_lowercase();
    // 去掉末尾標點再比對（含中英文標點）
    let s = s.trim_end_matches(['.', '!', '?', ',', '。', '！', '？', '…']).trim();
    matches!(
        s,
        // ── 英文 ────────────────────────────────────────────
        "thank you"
            | "thanks"
            | "thanks for watching"
            | "thank you for watching"
            | "thank you for listening"
            | "please subscribe"
            | "like and subscribe"
            | "subscribe"
            | "bye"
            | "bye bye"
            | "you"
            | "subtitles by the amara.org community"
        // ── 標記符號（各語言通用）────────────────────────────
            | "[silence]"
            | "[ silence ]"
            | "[blank_audio]"
            | "[ blank_audio ]"
            | "[music]"
            | "[ music ]"
            | "[applause]"
            | "[音乐]"
            | "[ 音乐 ]"
            | "[掌声]"
            | "[ 掌声 ]"
            | "[音樂]"
            | "[ 音樂 ]"
            | "[掌聲]"
            | "[ 掌聲 ]"
        // ── 繁體中文 ─────────────────────────────────────────
            | "謝謝"
            | "謝謝你"
            | "謝謝大家"
            | "謝謝觀看"
            | "謝謝收看"
            | "訂閱"
            | "請訂閱"
            | "按讚訂閱"
        // ── 簡體中文 ─────────────────────────────────────────
            | "谢谢"
            | "谢谢你"
            | "谢谢大家"
            | "谢谢观看"
            | "谢谢收看"
            | "订阅"
            | "请订阅"
            | "请关注"
    )
}

/// App 啟動時呼叫：若已設定路徑則背景預熱 whisper-server 並送出靜音推論
/// 不阻塞啟動流程；設定錯誤（路徑不存在等）會透過 whisper:stderr 事件通知前端
pub async fn warmup_whisper_server(state: &AppState, app: &AppHandle) {
    let tok_owned = state.get_auth_token().await;
    let tok: Option<&str> = if tok_owned.is_empty() { None } else { Some(&tok_owned) };
    let cli_configured = matches!(
        daemon_get_setting(&state.http_client, tok, "whisper_cli_path").await,
        Some(ref p) if !p.is_empty()
    );
    let model_configured = matches!(
        daemon_get_setting(&state.http_client, tok, "whisper_model_path").await,
        Some(ref p) if !p.is_empty()
    );
    if !cli_configured || !model_configured {
        return; // 未設定是正常情況，靜默跳過
    }
    match ensure_whisper_server_running(state, app).await {
        Ok(base_url) => {
            // 送一段靜音給 whisper 做推論預熱，觸發 Metal shader 編譯 / 內部緩衝區分配
            // 這樣使用者第一次錄音時推論速度就會與後續段落相同
            let _ = warmup_whisper_inference(&base_url).await;
            let _ = app.emit("whisper:stderr", "[server] 推論引擎預熱完成");
        }
        Err(e) => {
            // 設定錯誤（檔案不存在、路徑錯誤等）→ 透過事件通知前端顯示 toast
            let _ = app.emit("whisper:stderr", &format!("[server:error] {}", e));
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
    // 先設旗標，阻止 transcribe_audio 在本次 kill+wait 期間或之後重啟
    state.whisper_user_stopped.store(true, Ordering::SeqCst);

    let mut guard = state.whisper_server.lock().await;
    if let Some(mut child) = guard.take() {
        child.kill().await.ok();
        // 等待進程真正退出，確保下一次 health ping 不會再回應
        // （只送 SIGKILL 不 wait 的話，OS 需要數十 ms 才會清理進程，
        //   這段空窗期輪詢會看到 "running" 而誤判為重新連線）
        child.wait().await.ok();
    }
    // 清除 port，讓 get_whisper_server_status 直接回傳 stopped 而不再 ping
    *state.whisper_actual_port.lock().await = None;
    Ok(())
}

/// 查詢 whisper-server 狀態："running" | "loading" | "stopped"
#[tauri::command]
pub async fn get_whisper_server_status(state: State<'_, AppState>) -> Result<String, AppError> {
    // whisper_actual_port 為 None 代表本次 session 從未成功啟動過 server
    // （二進位不存在時 ensure_whisper_server_running 在 resolve_config 階段就失敗，
    //   不會設定 port）。這時不應去 ping 預設 8081 — 那會誤判孤立進程為 running。
    let port = match *state.whisper_actual_port.lock().await {
        Some(p) => p,
        None => {
            // 確認 child state 也是 None（一致性檢查），直接回傳 stopped
            return Ok("stopped".to_string());
        }
    };

    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::new();
    let healthy = client
        .get(format!("{}/health", base_url))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .is_ok();
    if healthy {
        return Ok("running".to_string());
    }
    let mut guard = state.whisper_server.lock().await;
    match guard.as_mut() {
        None => Ok("stopped".to_string()),
        Some(child) => match child.try_wait() {
            Ok(None) => Ok("loading".to_string()),
            _ => {
                *guard = None;
                Ok("stopped".to_string())
            }
        },
    }
}

/// 手動啟動 whisper-server
#[tauri::command]
pub async fn start_whisper_server(
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), AppError> {
    // 清除停止旗標，允許重新啟動
    state.whisper_user_stopped.store(false, Ordering::SeqCst);
    ensure_whisper_server_running(state.inner(), &app).await?;
    Ok(())
}

/// 重啟 whisper-server（先強制關閉再重新啟動）
#[tauri::command]
pub async fn restart_whisper_server(
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), AppError> {
    // 清除停止旗標，允許重新啟動
    state.whisper_user_stopped.store(false, Ordering::SeqCst);
    {
        let mut guard = state.whisper_server.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill().await.ok();
            // wait() 確保進程真正退出後 OS 才釋放 port，
            // 避免重啟時 orphan check 誤判舊進程仍存活
            child.wait().await.ok();
        }
    }
    ensure_whisper_server_running(state.inner(), &app).await?;
    Ok(())
}

// ─── WAV helper ───────────────────────────────────────────────────────────────

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

    buf
}
