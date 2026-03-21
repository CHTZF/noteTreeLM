use crate::{db::queries, error::AppError, state::AppState};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};
use tokio::io::AsyncReadExt;

/// 從可用 port 開始往上尋找第一個空閒的 localhost port
pub fn find_free_port(preferred: u16) -> u16 {
    use std::net::TcpListener;
    for port in preferred..=65535 {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    preferred
}

/// 從 DB 讀取 llama-server 路徑、模型路徑（port 由執行時自動分配）
async fn resolve_server_config(state: &AppState) -> Result<(PathBuf, String), AppError> {
    let db = &state.db;

    let server_path = queries::get_setting(db, "llama_cli_path")
        .await?
        .unwrap_or_default();
    let server_path = server_path.trim().trim_matches('"').trim_matches('\'').to_string();

    let model_path = queries::get_setting(db, "llm_model_path")
        .await?
        .unwrap_or_default();
    let model_path = model_path.trim().trim_matches('"').trim_matches('\'').to_string();

    if server_path.is_empty() {
        return Err(AppError::AI(
            "尚未設定 llama-server 路徑，請到 Settings > AI 設定。".to_string(),
        ));
    }
    if model_path.is_empty() {
        return Err(AppError::AI(
            "尚未設定本地 LLM 模型路徑，請到 Settings > AI 設定。".to_string(),
        ));
    }

    #[cfg(not(windows))]
    let bin = PathBuf::from(&server_path);
    #[cfg(windows)]
    let bin = {
        let b = PathBuf::from(&server_path);
        if !b.exists() && b.extension().is_none() {
            let candidate = b.with_extension("exe");
            if candidate.exists() { candidate } else { b }
        } else { b }
    };
    if !bin.exists() {
        return Err(AppError::AI(format!(
            "找不到 llama-server：{}",
            bin.display()
        )));
    }

    Ok((bin, model_path))
}

/// 確保 llama-server 正在運行；若未啟動則自動 spawn
/// 回傳 base URL（例如 "http://127.0.0.1:8080"）
pub(crate) async fn ensure_server_running(
    state: &AppState,
    app: &AppHandle,
) -> Result<String, AppError> {
    if state.llama_user_stopped.load(Ordering::SeqCst) {
        return Err(AppError::AI("llama-server 已手動停止".to_string()));
    }

    let _start_lock = state.llama_start_lock.lock().await;

    let (bin, model_path) = resolve_server_config(state).await?;

    let port = {
        let _alloc_lock = state.port_allocator.lock().await;
        let mut guard = state.llama_actual_port.lock().await;
        if let Some(p) = *guard {
            p
        } else {
            let p = find_free_port(8080);
            *guard = Some(p);
            p
        }
    };

    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::new();

    {
        let guard = state.llama_server.lock().await;
        if guard.is_some() {
            let alive = client
                .get(format!("{}/health", base_url))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if alive {
                return Ok(base_url);
            }
            let _ = app.emit("llm:stderr", "[server] 伺服器意外退出，重新啟動…");
        }
    }

    let _ = app.emit(
        "llm:stderr",
        format!(
            "[server] 啟動 llama-server：{}\n  模型：{}\n  埠：{}",
            bin.display(),
            model_path,
            port
        ),
    );

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args([
            "--model",
            &model_path,
            "--port",
            &port.to_string(),
            "--host",
            "127.0.0.1",
            "--ctx-size",
            "8192",
            "--parallel",
            "1",
            "--n-gpu-layers", "99",  // Metal/CUDA/Vulkan offload; llama.cpp 自動降回 CPU（無 GPU 時不 crash）
            "--embedding",
            "--pooling",
            "mean",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::AI(format!("llama-server 啟動失敗：{}", e)))?;

    if let Some(mut stderr) = child.stderr.take() {
        let app_stderr = app.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                        let _ = app_stderr.emit("llm:stderr", &chunk);
                    }
                }
            }
        });
    }

    {
        let mut guard = state.llama_server.lock().await;
        *guard = Some(child);
    }

    let _ = app.emit("llm:stderr", "[server] 等待模型載入…");
    for i in 0..60u32 {
        tokio::time::sleep(Duration::from_secs(1)).await;

        {
            let mut guard = state.llama_server.lock().await;
            match guard.as_mut() {
                None => {
                    return Err(AppError::AI("llama-server 已手動停止".to_string()));
                }
                Some(child) => {
                    if let Ok(Some(status)) = child.try_wait() {
                        *guard = None;
                        let hint = match status.code() {
                            Some(-1073741515) => " 缺少必要的 DLL，請安裝 Visual C++ Redistributable (x64) 或確認 llama-server 版本與 CPU/CUDA 相容。",
                            Some(-1073741701) => " 二進位格式不符，請確認 llama-server 為 64-bit Windows 版本。",
                            _ => "",
                        };
                        return Err(AppError::AI(format!(
                            "llama-server 意外退出（code: {:?}），請確認模型路徑與二進位設定。{}",
                            status.code(), hint
                        )));
                    }
                }
            }
        }

        let ready = client
            .get(format!("{}/health", base_url))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);

        if ready {
            let _ = app.emit(
                "llm:stderr",
                format!("[server] 就緒（等待 {} 秒）", i + 1),
            );
            return Ok(base_url);
        }

        if i > 0 && i % 10 == 9 {
            let _ = app.emit(
                "llm:stderr",
                format!("[server] 載入中…（已等待 {} 秒）", i + 1),
            );
        }
    }

    Err(AppError::AI(
        "llama-server 啟動超時（60 秒），請確認 llama-server 路徑與模型設定。".to_string(),
    ))
}

/// 呼叫 llama-server embedding endpoint，取得單一文字的 embedding 向量
/// 依序嘗試：/v1/embeddings（OpenAI 格式）→ /embedding（legacy 格式）
/// 失敗時回傳空 Vec（非致命錯誤，由呼叫端 fallback）
pub async fn get_embedding(client: &reqwest::Client, base_url: &str, text: &str) -> Vec<f32> {

    fn extract_vec(json: &serde_json::Value) -> Vec<f32> {
        if let Some(arr) = json["data"][0]["embedding"].as_array() {
            let v: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
            if !v.is_empty() { return v; }
        }
        if let Some(arr) = json["embedding"].as_array() {
            let v: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
            if !v.is_empty() { return v; }
        }
        if let Some(first) = json.as_array().and_then(|a| a.first()) {
            if let Some(arr) = first["embedding"].as_array() {
                let v: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
                if !v.is_empty() { return v; }
            }
        }
        vec![]
    }

    if let Ok(resp) = client
        .post(format!("{}/v1/embeddings", base_url))
        .json(&serde_json::json!({ "input": text }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let v = extract_vec(&json);
                if !v.is_empty() { return v; }
            }
        }
    }

    if let Ok(resp) = client
        .post(format!("{}/v1/embeddings", base_url))
        .json(&serde_json::json!({ "input": text, "model": "embedding" }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let v = extract_vec(&json);
                if !v.is_empty() { return v; }
            }
        }
    }

    if let Ok(resp) = client
        .post(format!("{}/embeddings", base_url))
        .json(&serde_json::json!({ "input": text }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let v = extract_vec(&json);
                if !v.is_empty() { return v; }
            }
        }
    }

    if let Ok(resp) = client
        .post(format!("{}/embedding", base_url))
        .json(&serde_json::json!({ "content": text }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let v = extract_vec(&json);
                if !v.is_empty() { return v; }
            }
        }
    }

    if let Ok(resp) = client
        .post(format!("{}/embedding", base_url))
        .json(&serde_json::json!({ "input": text }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                return extract_vec(&json);
            }
        }
    }

    vec![]
}

/// Batch embedding: send multiple texts in one HTTP request.
///
/// Uses the OpenAI `/v1/embeddings` format with `"input": [...]`.
/// Returns one `Vec<f32>` per input text (empty vec if that text failed).
/// Falls back to individual `get_embedding` calls if the batch endpoint
/// returns an unexpected format.
pub async fn get_embeddings_batch(
    client: &reqwest::Client,
    base_url: &str,
    texts: &[&str],
) -> Vec<Vec<f32>> {
    if texts.is_empty() {
        return vec![];
    }

    // Helper: parse a batch response `data[].embedding` sorted by `index`.
    fn extract_batch(json: &serde_json::Value, n: usize) -> Option<Vec<Vec<f32>>> {
        let data = json["data"].as_array()?;
        if data.is_empty() { return None; }
        let mut out: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
        for item in data {
            let idx = item["index"].as_u64().unwrap_or(0) as usize;
            if let Some(arr) = item["embedding"].as_array() {
                let v: Vec<f32> = arr.iter()
                    .filter_map(|x| x.as_f64().map(|f| f as f32))
                    .collect();
                if !v.is_empty() { out.push((idx, v)); }
            }
        }
        if out.is_empty() { return None; }
        out.sort_by_key(|(i, _)| *i);
        // Fill any missing indices with empty vecs
        let mut result = vec![vec![]; n];
        for (idx, v) in out {
            if idx < n { result[idx] = v; }
        }
        Some(result)
    }

    let input = serde_json::json!(texts);

    // Try /v1/embeddings with array input (primary)
    for payload in [
        serde_json::json!({ "input": input }),
        serde_json::json!({ "input": input, "model": "embedding" }),
    ] {
        if let Ok(resp) = client
            .post(format!("{}/v1/embeddings", base_url))
            .json(&payload)
            .timeout(Duration::from_secs(120))
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    if let Some(batch) = extract_batch(&json, texts.len()) {
                        return batch;
                    }
                }
            }
        }
    }

    // Fallback: individual requests (preserves correct ordering)
    let futs: Vec<_> = texts.iter().map(|t| {
        let client = client.clone();
        let base = base_url.to_owned();
        let text = t.to_string();
        async move { get_embedding(&client, &base, &text).await }
    }).collect();
    futures::future::join_all(futs).await
}

/// App 啟動時呼叫：若已設定路徑則背景預熱 llama-server
pub async fn warmup_llama_server(state: &AppState, app: &AppHandle) {
    let configured = matches!(
        queries::get_setting(&state.db, "llama_cli_path").await,
        Ok(Some(ref p)) if !p.is_empty()
    ) && matches!(
        queries::get_setting(&state.db, "llm_model_path").await,
        Ok(Some(ref p)) if !p.is_empty()
    );
    if !configured {
        return;
    }
    if let Err(e) = ensure_server_running(state, app).await {
        let _ = app.emit("llm:stderr", format!("[server:error] {}", e));
    }
}

/// 手動停止 llama-server（App 退出時也會自動呼叫）
#[tauri::command]
pub async fn stop_llama_server(state: State<'_, AppState>) -> Result<(), AppError> {
    state.llama_user_stopped.store(true, Ordering::SeqCst);

    let mut guard = state.llama_server.lock().await;
    if let Some(mut child) = guard.take() {
        child.kill().await.ok();
        child.wait().await.ok();
    }
    *state.llama_actual_port.lock().await = None;
    Ok(())
}

/// 查詢 llama-server 狀態："running" | "loading" | "stopped"
#[tauri::command]
pub async fn get_llama_server_status(state: State<'_, AppState>) -> Result<String, AppError> {
    let port = match *state.llama_actual_port.lock().await {
        Some(p) => p,
        None => return Ok("stopped".to_string()),
    };
    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::new();
    let healthy = client
        .get(format!("{}/health", base_url))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if healthy {
        return Ok("running".to_string());
    }
    let mut guard = state.llama_server.lock().await;
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

/// 手動啟動 llama-server
#[tauri::command]
pub async fn start_llama_server(
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), AppError> {
    state.llama_user_stopped.store(false, Ordering::SeqCst);
    ensure_server_running(state.inner(), &app).await?;
    Ok(())
}

/// 重啟 llama-server（先強制關閉再重新啟動）
#[tauri::command]
pub async fn restart_llama_server(
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), AppError> {
    state.llama_user_stopped.store(false, Ordering::SeqCst);
    {
        let mut guard = state.llama_server.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill().await.ok();
            child.wait().await.ok();
        }
    }
    ensure_server_running(state.inner(), &app).await?;
    Ok(())
}

// ─── Embedding Server ─────────────────────────────────────────────────────────

async fn resolve_embedding_config(state: &AppState) -> Result<(PathBuf, String), AppError> {
    let db = &state.db;
    let server_path = queries::get_setting(db, "llama_cli_path")
        .await?
        .unwrap_or_default();
    let model_path = queries::get_setting(db, "embedding_model_path")
        .await?
        .unwrap_or_default();
    if server_path.is_empty() {
        return Err(AppError::AI("尚未設定 llama-server 執行檔路徑".to_string()));
    }
    if model_path.is_empty() {
        return Err(AppError::AI("尚未設定 Embedding 模型路徑".to_string()));
    }
    #[cfg(not(windows))]
    let bin = PathBuf::from(&server_path);
    #[cfg(windows)]
    let bin = {
        let b = PathBuf::from(&server_path);
        if !b.exists() && b.extension().is_none() {
            let candidate = b.with_extension("exe");
            if candidate.exists() { candidate } else { b }
        } else { b }
    };
    if !bin.exists() {
        return Err(AppError::AI(format!("找不到 llama-server：{}", bin.display())));
    }
    Ok((bin, model_path))
}

/// 確保 embedding-server 正在運行；若未啟動則自動 spawn
/// 回傳 base URL（例如 "http://127.0.0.1:8082"）
pub(crate) async fn ensure_embedding_server_running(
    state: &AppState,
    app: &AppHandle,
) -> Result<String, AppError> {
    if state.embedding_user_stopped.load(Ordering::SeqCst) {
        return Err(AppError::AI("embedding-server 已手動停止".to_string()));
    }
    let _start_lock = state.embedding_start_lock.lock().await;
    let (bin, model_path) = resolve_embedding_config(state).await?;

    let port = {
        let _alloc_lock = state.port_allocator.lock().await;
        let mut guard = state.embedding_actual_port.lock().await;
        if let Some(p) = *guard {
            p
        } else {
            let p = find_free_port(8082);
            *guard = Some(p);
            p
        }
    };

    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::new();

    {
        let guard = state.embedding_server.lock().await;
        if guard.is_some() {
            let alive = client
                .get(format!("{}/health", base_url))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if alive { return Ok(base_url); }
            let _ = app.emit("llm:stderr", "[embed] 伺服器意外退出，重新啟動…");
        }
    }

    let _ = app.emit("llm:stderr", format!(
        "[embed] 啟動 embedding-server：{}\n  模型：{}\n  埠：{}", bin.display(), model_path, port
    ));

    let cpu_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .to_string();

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args([
        "--model",       &model_path,
        "--port",        &port.to_string(),
        "--host",        "127.0.0.1",
        "--ctx-size",    "512",
        "--batch-size",  "2048",   // parallel(4) × ctx-size(512) — 一次 forward pass 的 token 上限
        "--parallel",    "4",      // 4 個 text 同時塞入一次 forward pass
        "--threads",     &cpu_threads,
        "--n-gpu-layers","99",     // Apple Silicon Metal 全層 offload（無 GPU 時 llama.cpp 自動降回 CPU）
        "--embeddings",
        "--embedding",
        "--pooling",     "cls",
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let mut child = cmd.spawn()
        .map_err(|e| AppError::AI(format!("embedding-server 啟動失敗：{}", e)))?;

    if let Some(mut stderr) = child.stderr.take() {
        let app_stderr = app.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => { let _ = app_stderr.emit("llm:stderr", String::from_utf8_lossy(&buf[..n]).as_ref()); }
                }
            }
        });
    }

    { *state.embedding_server.lock().await = Some(child); }

    let _ = app.emit("llm:stderr", "[embed] 等待模型載入…");
    for i in 0..60u32 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        {
            let mut guard = state.embedding_server.lock().await;
            match guard.as_mut() {
                None => return Err(AppError::AI("embedding-server 已手動停止".to_string())),
                Some(child) => {
                    if let Ok(Some(_)) = child.try_wait() {
                        *guard = None;
                        return Err(AppError::AI("embedding-server 意外退出，請確認模型路徑。".to_string()));
                    }
                }
            }
        }
        let ready = client
            .get(format!("{}/health", base_url))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ready {
            let _ = app.emit("llm:stderr", format!("[embed] 就緒（等待 {} 秒）", i + 1));
            return Ok(base_url);
        }
    }
    Err(AppError::AI("embedding-server 啟動超時（60 秒）".to_string()))
}

/// App 啟動時預熱 embedding-server（若已設定模型）
pub async fn warmup_embedding_server(state: &AppState, app: &AppHandle) {
    let configured = matches!(
        queries::get_setting(&state.db, "llama_cli_path").await,
        Ok(Some(ref p)) if !p.is_empty()
    ) && matches!(
        queries::get_setting(&state.db, "embedding_model_path").await,
        Ok(Some(ref p)) if !p.is_empty()
    );
    if !configured { return; }
    if let Err(e) = ensure_embedding_server_running(state, app).await {
        let _ = app.emit("llm:stderr", format!("[embed:error] {}", e));
    }
}

#[tauri::command]
pub async fn get_embedding_server_status(state: State<'_, AppState>) -> Result<String, AppError> {
    let port = match *state.embedding_actual_port.lock().await {
        Some(p) => p,
        None => return Ok("stopped".to_string()),
    };
    let base_url = format!("http://127.0.0.1:{}", port);
    let healthy = reqwest::Client::new()
        .get(format!("{}/health", base_url))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if healthy { return Ok("running".to_string()); }
    let mut guard = state.embedding_server.lock().await;
    match guard.as_mut() {
        None => Ok("stopped".to_string()),
        Some(child) => match child.try_wait() {
            Ok(None) => Ok("loading".to_string()),
            _ => { *guard = None; Ok("stopped".to_string()) }
        },
    }
}

/// 診斷用：測試 embedding server 的端點，回傳狀態碼與回應摘要
#[tauri::command]
pub async fn check_embedding_endpoint(state: State<'_, AppState>) -> Result<String, AppError> {
    let port = match *state.embedding_actual_port.lock().await {
        Some(p) => p,
        None => return Ok("embedding_actual_port = None（server 未在 state 中登記）".to_string()),
    };
    let base_url = format!("http://127.0.0.1:{}", port);
    let client = reqwest::Client::new();
    let mut out = format!("base_url: {}\n", base_url);

    match client.get(format!("{}/health", base_url)).timeout(Duration::from_secs(5)).send().await {
        Err(e) => { out += &format!("GET /health: 失敗 — {}\n", e); }
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let snippet = if text.len() > 200 { format!("{}…", &text[..200]) } else { text };
            out += &format!("GET /health: HTTP {} | {}\n", status, snippet);
        }
    }

    for (label, url, body) in [
        ("/v1/embeddings",            format!("{}/v1/embeddings", base_url), serde_json::json!({"input":"test"})),
        ("/v1/embeddings+model",      format!("{}/v1/embeddings", base_url), serde_json::json!({"input":"test","model":"embedding"})),
        ("/embeddings",               format!("{}/embeddings",    base_url), serde_json::json!({"input":"test"})),
        ("/embedding(content)",       format!("{}/embedding",     base_url), serde_json::json!({"content":"test"})),
        ("/embedding(input)",         format!("{}/embedding",     base_url), serde_json::json!({"input":"test"})),
    ] {
        match client.post(&url).json(&body).timeout(Duration::from_secs(10)).send().await {
            Err(e) => { out += &format!("POST {}: 請求失敗 — {}\n", label, e); }
            Ok(resp) => {
                let status = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                let snippet = if text.len() > 300 { format!("{}…", &text[..300]) } else { text };
                out += &format!("POST {}: HTTP {} | {}\n", label, status, snippet);
            }
        }
    }

    let has_child = state.embedding_server.lock().await.is_some();
    out += &format!("embedding_server child process: {}\n", if has_child { "Some（有子進程）" } else { "None（無子進程）" });

    Ok(out)
}

#[tauri::command]
pub async fn start_embedding_server(state: State<'_, AppState>, app: AppHandle) -> Result<(), AppError> {
    state.embedding_user_stopped.store(false, Ordering::SeqCst);
    ensure_embedding_server_running(state.inner(), &app).await?;
    Ok(())
}

#[tauri::command]
pub async fn stop_embedding_server(state: State<'_, AppState>) -> Result<(), AppError> {
    state.embedding_user_stopped.store(true, Ordering::SeqCst);
    let mut guard = state.embedding_server.lock().await;
    if let Some(mut child) = guard.take() {
        child.kill().await.ok();
        child.wait().await.ok();
    }
    *state.embedding_actual_port.lock().await = None;
    Ok(())
}

#[tauri::command]
pub async fn restart_embedding_server(state: State<'_, AppState>, app: AppHandle) -> Result<(), AppError> {
    state.embedding_user_stopped.store(false, Ordering::SeqCst);
    {
        let mut guard = state.embedding_server.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill().await.ok();
            child.wait().await.ok();
        }
    }
    ensure_embedding_server_running(state.inner(), &app).await?;
    Ok(())
}
