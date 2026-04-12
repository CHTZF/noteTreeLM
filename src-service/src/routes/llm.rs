use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::atomic::Ordering, time::Duration};

use crate::app_state::ApiState;
use crate::db::SurrealDb;
use crate::routes::whisper::build_silent_wav;

pub fn router() -> Router<ApiState> {
    Router::new()
        // llama-server lifecycle
        .route("/llm/status",  get(llama_status_handler))
        .route("/llm/start",   post(llama_start_handler))
        .route("/llm/stop",    post(llama_stop_handler))
        .route("/llm/restart", post(llama_restart_handler))
        // embedding-server lifecycle
        .route("/embedding/status",  get(embedding_status_handler))
        .route("/embedding/start",   post(embedding_start_handler))
        .route("/embedding/stop",    post(embedding_stop_handler))
        .route("/embedding/restart", post(embedding_restart_handler))
        // LLM inference endpoints (called by Tauri instead of hitting llama directly)
        .route("/llm/chat",             post(chat_handler))
        .route("/llm/embedding",        post(single_embedding_handler))
        .route("/llm/embeddings/batch", post(batch_embedding_handler))
}

// ─── Settings ─────────────────────────────────────────────────────────────────

async fn get_setting(db: &SurrealDb, key: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Row { value: String }
    db.query("SELECT `value` FROM `settings` WHERE `key` = $key LIMIT 1")
        .bind(("key", key.to_string()))
        .await.ok()?
        .take::<Vec<Row>>(0).ok()?
        .into_iter().next()
        .map(|r| r.value)
        .filter(|v| !v.is_empty())
}

// ─── llama-server lifecycle ───────────────────────────────────────────────────

struct LlamaConfig {
    bin: PathBuf,
    model_path: String,
}

async fn resolve_llama_config(db: &SurrealDb) -> Result<LlamaConfig, String> {
    let cli_path = get_setting(db, "llama_cli_path").await.unwrap_or_default();
    let cli_path = cli_path.trim().trim_matches('"').trim_matches('\'').to_string();
    let model_path = get_setting(db, "llm_model_path").await.unwrap_or_default();
    let model_path = model_path.trim().trim_matches('"').trim_matches('\'').to_string();

    if cli_path.is_empty() {
        return Err("尚未設定 llama-server 路徑，請到 Settings > AI 設定。".to_string());
    }
    if model_path.is_empty() {
        return Err("尚未設定本地 LLM 模型路徑，請到 Settings > AI 設定。".to_string());
    }

    #[cfg(not(windows))]
    let bin = PathBuf::from(&cli_path);
    #[cfg(windows)]
    let bin = {
        let b = PathBuf::from(&cli_path);
        if !b.exists() && b.extension().is_none() {
            let c = b.with_extension("exe");
            if c.exists() { c } else { b }
        } else { b }
    };
    if !bin.exists() {
        return Err(format!("找不到 llama-server：{}，請到 Settings > AI 更新路徑。", bin.display()));
    }

    Ok(LlamaConfig { bin, model_path })
}

pub(crate) async fn ensure_llama_running(state: &ApiState) -> Result<String, String> {
    if state.daemon.llama_user_stopped.load(Ordering::SeqCst) {
        return Err("llama-server 已手動停止".to_string());
    }

    let _lock = state.daemon.llama_start_lock.lock().await;
    let config = resolve_llama_config(&state.db).await?;

    let base_url = state.daemon.llm_url.clone();
    let client = reqwest::Client::new();

    // Already healthy?
    {
        let guard = state.daemon.llama_server.lock().await;
        if guard.is_some() {
            let alive = client.get(format!("{}/health", base_url))
                .timeout(Duration::from_secs(2)).send().await
                .map(|r| r.status().is_success()).unwrap_or(false);
            if alive {
                return Ok(base_url);
            }
            state.daemon.emit("llm:stderr", json!("[server] 伺服器意外退出，重新啟動…"));
        }
    }

    let port = state.daemon.llm_url.rsplit(':').next()
        .and_then(|p| p.parse::<u16>().ok()).unwrap_or(18080);

    state.daemon.emit("llm:stderr", json!(format!(
        "[server] 啟動 llama-server：{}\n  模型：{}\n  埠：{}",
        config.bin.display(), config.model_path, port
    )));

    let ctx_size = crate::service::ctx_size_for_model(&config.model_path) as u32;
    let ctx_size_str = ctx_size.to_string();

    let mut cmd = tokio::process::Command::new(&config.bin);
    cmd.args([
        "--model",        &config.model_path,
        "--port",         &port.to_string(),
        "--host",         "127.0.0.1",
        "--ctx-size",     &ctx_size_str,
        "--parallel",     "1",
        "--n-gpu-layers", "99",
        "--cache-type-k", "q8_0",
        "--embedding",
        "--pooling",      "mean",
    ]);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    #[cfg(target_os = "windows")]
    { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }

    let mut child = cmd.spawn()
        .map_err(|e| format!("llama-server 啟動失敗：{}", e))?;

    if let Some(stderr) = child.stderr.take() {
        let tx = state.daemon.event_tx.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(crate::service::types::ServiceEvent {
                    event: "llm:stderr".to_string(),
                    payload: json!(line),
                });
            }
        });
    }

    *state.daemon.llama_server.lock().await = Some(child);

    state.daemon.emit("llm:stderr", json!("[server] 等待模型載入…"));
    for i in 0..60u32 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        {
            let mut guard = state.daemon.llama_server.lock().await;
            match guard.as_mut() {
                None => return Err("llama-server 已手動停止".to_string()),
                Some(child) => {
                    if let Ok(Some(status)) = child.try_wait() {
                        *guard = None;
                        let hint = match status.code() {
                            Some(-1073741515) => " 缺少必要的 DLL，請安裝 Visual C++ Redistributable。",
                            Some(-1073741701) => " 二進位格式不符，請確認為 64-bit Windows 版本。",
                            _ => "",
                        };
                        return Err(format!(
                            "llama-server 意外退出（code: {:?}），請確認模型路徑與二進位設定。{}",
                            status.code(), hint
                        ));
                    }
                }
            }
        }

        let ready = client.get(format!("{}/health", base_url))
            .timeout(Duration::from_secs(2)).send().await
            .map(|r| r.status().is_success()).unwrap_or(false);
        if ready {
            state.daemon.emit("llm:stderr", json!(format!("[server] 就緒（等待 {} 秒）", i + 1)));
            return Ok(base_url);
        }
        if i > 0 && i % 10 == 9 {
            state.daemon.emit("llm:stderr", json!(format!("[server] 載入中…（已等待 {} 秒）", i + 1)));
        }
    }
    Err("llama-server 啟動超時（60 秒），請確認 llama-server 路徑與模型設定。".to_string())
}

// ─── embedding-server lifecycle ───────────────────────────────────────────────

struct EmbeddingConfig {
    bin: PathBuf,
    model_path: String,
}

async fn resolve_embedding_config(db: &SurrealDb) -> Result<EmbeddingConfig, String> {
    let cli_path = get_setting(db, "llama_cli_path").await.unwrap_or_default();
    let cli_path = cli_path.trim().trim_matches('"').trim_matches('\'').to_string();
    let model_path = get_setting(db, "embedding_model_path").await.unwrap_or_default();
    let model_path = model_path.trim().trim_matches('"').trim_matches('\'').to_string();

    if cli_path.is_empty() {
        return Err("尚未設定 llama-server 執行檔路徑".to_string());
    }
    if model_path.is_empty() {
        return Err("尚未設定 Embedding 模型路徑".to_string());
    }

    #[cfg(not(windows))]
    let bin = PathBuf::from(&cli_path);
    #[cfg(windows)]
    let bin = {
        let b = PathBuf::from(&cli_path);
        if !b.exists() && b.extension().is_none() {
            let c = b.with_extension("exe");
            if c.exists() { c } else { b }
        } else { b }
    };
    if !bin.exists() {
        return Err(format!("找不到 llama-server：{}", bin.display()));
    }

    Ok(EmbeddingConfig { bin, model_path })
}

async fn ensure_embedding_running(state: &ApiState) -> Result<String, String> {
    if state.daemon.embedding_user_stopped.load(Ordering::SeqCst) {
        return Err("embedding-server 已手動停止".to_string());
    }

    let _lock = state.daemon.embedding_start_lock.lock().await;
    let config = resolve_embedding_config(&state.db).await?;

    let base_url = state.daemon.embedding_url.clone();
    let client = reqwest::Client::new();

    {
        let guard = state.daemon.embedding_server.lock().await;
        if guard.is_some() {
            let alive = client.get(format!("{}/health", base_url))
                .timeout(Duration::from_secs(2)).send().await
                .map(|r| r.status().is_success()).unwrap_or(false);
            if alive {
                return Ok(base_url);
            }
            state.daemon.emit("llm:stderr", json!("[embed] 伺服器意外退出，重新啟動…"));
        }
    }

    let port = state.daemon.embedding_url.rsplit(':').next()
        .and_then(|p| p.parse::<u16>().ok()).unwrap_or(18081);

    state.daemon.emit("llm:stderr", json!(format!(
        "[embed] 啟動 embedding-server：{}\n  模型：{}\n  埠：{}",
        config.bin.display(), config.model_path, port
    )));

    let cpu_threads = std::thread::available_parallelism()
        .map(|n| n.get()).unwrap_or(4).to_string();

    let mut cmd = tokio::process::Command::new(&config.bin);
    cmd.args([
        "--model",        &config.model_path,
        "--port",         &port.to_string(),
        "--host",         "127.0.0.1",
        "--ctx-size",     "512",
        "--batch-size",   "2048",
        "--parallel",     "4",
        "--threads",      &cpu_threads,
        "--n-gpu-layers", "99",
        "--embeddings",
        "--embedding",
        "--pooling",      "cls",
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::piped());
    #[cfg(target_os = "windows")]
    { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }

    let mut child = cmd.spawn()
        .map_err(|e| format!("embedding-server 啟動失敗：{}", e))?;

    if let Some(stderr) = child.stderr.take() {
        let tx = state.daemon.event_tx.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(crate::service::types::ServiceEvent {
                    event: "llm:stderr".to_string(),
                    payload: json!(line),
                });
            }
        });
    }

    *state.daemon.embedding_server.lock().await = Some(child);

    state.daemon.emit("llm:stderr", json!("[embed] 等待模型載入…"));
    for i in 0..60u32 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        {
            let mut guard = state.daemon.embedding_server.lock().await;
            match guard.as_mut() {
                None => return Err("embedding-server 已手動停止".to_string()),
                Some(child) => {
                    if let Ok(Some(_)) = child.try_wait() {
                        *guard = None;
                        return Err("embedding-server 意外退出，請確認模型路徑。".to_string());
                    }
                }
            }
        }
        let ready = client.get(format!("{}/health", base_url))
            .timeout(Duration::from_secs(2)).send().await
            .map(|r| r.status().is_success()).unwrap_or(false);
        if ready {
            state.daemon.emit("llm:stderr", json!(format!("[embed] 就緒（等待 {} 秒）", i + 1)));
            return Ok(base_url);
        }
    }
    Err("embedding-server 啟動超時（60 秒）".to_string())
}

// ─── Embedding helper (used internally and exported for other modules) ────────

pub async fn compute_embedding(state: &ApiState, text: &str) -> Vec<f32> {
    let base_url = match ensure_embedding_running(state).await {
        Ok(u) => u,
        Err(_) => return vec![],
    };
    get_embedding_from_url(&reqwest::Client::new(), &base_url, text).await
}

async fn get_embedding_from_url(client: &reqwest::Client, base_url: &str, text: &str) -> Vec<f32> {
    fn extract(json: &Value) -> Vec<f32> {
        for path in [
            json["data"][0]["embedding"].as_array(),
            json["embedding"].as_array(),
            json.as_array().and_then(|a| a.first()).and_then(|v| v["embedding"].as_array()),
        ].into_iter().flatten() {
            let v: Vec<f32> = path.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
            if !v.is_empty() { return v; }
        }
        vec![]
    }

    for (url, body) in [
        (format!("{}/v1/embeddings", base_url), json!({"input": text})),
        (format!("{}/v1/embeddings", base_url), json!({"input": text, "model": "embedding"})),
        (format!("{}/embeddings",    base_url), json!({"input": text})),
        (format!("{}/embedding",     base_url), json!({"content": text})),
        (format!("{}/embedding",     base_url), json!({"input": text})),
    ] {
        if let Ok(resp) = client.post(&url).json(&body).timeout(Duration::from_secs(30)).send().await {
            if resp.status().is_success() {
                if let Ok(j) = resp.json::<Value>().await {
                    let v = extract(&j);
                    if !v.is_empty() { return v; }
                }
            }
        }
    }
    vec![]
}

// ─── Warmup ───────────────────────────────────────────────────────────────────

async fn warmup_llama_inference(base_url: &str) {
    let client = reqwest::Client::new();
    let _ = client.post(format!("{}/v1/chat/completions", base_url))
        .json(&json!({"messages":[{"role":"user","content":"hi"}],"max_tokens":1,"stream":false}))
        .timeout(Duration::from_secs(60))
        .send().await;
}

async fn warmup_embedding_inference(base_url: &str) {
    let client = reqwest::Client::new();
    let wav = build_silent_wav(0.1, 16000);
    let _ = client.post(format!("{}/v1/embeddings", base_url))
        .json(&json!({"input": std::str::from_utf8(&wav[..8]).unwrap_or("test")}))
        .timeout(Duration::from_secs(30))
        .send().await;
    // Simpler: just send a text embedding warmup
    let _ = client.post(format!("{}/v1/embeddings", base_url))
        .json(&json!({"input": "warmup"}))
        .timeout(Duration::from_secs(30))
        .send().await;
}

// ─── Lifecycle route handlers ─────────────────────────────────────────────────

async fn llama_status_handler(State(state): State<ApiState>) -> Json<Value> {
    let healthy = reqwest::Client::new()
        .get(format!("{}/health", state.daemon.llm_url))
        .timeout(Duration::from_secs(2)).send().await
        .map(|r| r.status().is_success()).unwrap_or(false);
    if healthy { return Json(json!({"status": "running"})); }
    let mut guard = state.daemon.llama_server.lock().await;
    match guard.as_mut() {
        None => Json(json!({"status": "stopped"})),
        Some(child) => match child.try_wait() {
            Ok(None) => Json(json!({"status": "loading"})),
            _ => { *guard = None; Json(json!({"status": "stopped"})) }
        },
    }
}

async fn llama_start_handler(State(state): State<ApiState>) -> Result<Json<Value>, (StatusCode, String)> {
    state.daemon.llama_user_stopped.store(false, Ordering::SeqCst);
    let cli_ok = get_setting(&state.db, "llama_cli_path").await.is_some();
    let model_ok = get_setting(&state.db, "llm_model_path").await.is_some();
    if !cli_ok || !model_ok {
        return Ok(Json(json!({"ok": true, "skipped": "not_configured"})));
    }
    let base_url = ensure_llama_running(&state).await.map_err(|e| {
        state.daemon.emit("llm:stderr", json!(format!("[server:error] {}", e)));
        (StatusCode::INTERNAL_SERVER_ERROR, e)
    })?;
    warmup_llama_inference(&base_url).await;
    state.daemon.emit("llm:stderr", json!("[server] 預熱完成"));
    Ok(Json(json!({"ok": true, "url": base_url})))
}

async fn llama_stop_handler(State(state): State<ApiState>) -> Json<Value> {
    state.daemon.llama_user_stopped.store(true, Ordering::SeqCst);
    let mut guard = state.daemon.llama_server.lock().await;
    if let Some(mut child) = guard.take() {
        child.kill().await.ok();
        child.wait().await.ok();
    }
    Json(json!({"ok": true}))
}

async fn llama_restart_handler(State(state): State<ApiState>) -> Result<Json<Value>, (StatusCode, String)> {
    state.daemon.llama_user_stopped.store(false, Ordering::SeqCst);
    {
        let mut guard = state.daemon.llama_server.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill().await.ok();
            child.wait().await.ok();
        }
    }
    let base_url = ensure_llama_running(&state).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    warmup_llama_inference(&base_url).await;
    Ok(Json(json!({"ok": true})))
}

async fn embedding_status_handler(State(state): State<ApiState>) -> Json<Value> {
    let healthy = reqwest::Client::new()
        .get(format!("{}/health", state.daemon.embedding_url))
        .timeout(Duration::from_secs(2)).send().await
        .map(|r| r.status().is_success()).unwrap_or(false);
    if healthy { return Json(json!({"status": "running"})); }
    let mut guard = state.daemon.embedding_server.lock().await;
    match guard.as_mut() {
        None => Json(json!({"status": "stopped"})),
        Some(child) => match child.try_wait() {
            Ok(None) => Json(json!({"status": "loading"})),
            _ => { *guard = None; Json(json!({"status": "stopped"})) }
        },
    }
}

async fn embedding_start_handler(State(state): State<ApiState>) -> Result<Json<Value>, (StatusCode, String)> {
    state.daemon.embedding_user_stopped.store(false, Ordering::SeqCst);
    let cli_ok = get_setting(&state.db, "llama_cli_path").await.is_some();
    let model_ok = get_setting(&state.db, "embedding_model_path").await.is_some();
    if !cli_ok || !model_ok {
        return Ok(Json(json!({"ok": true, "skipped": "not_configured"})));
    }
    let base_url = ensure_embedding_running(&state).await.map_err(|e| {
        state.daemon.emit("llm:stderr", json!(format!("[embed:error] {}", e)));
        (StatusCode::INTERNAL_SERVER_ERROR, e)
    })?;
    warmup_embedding_inference(&base_url).await;
    state.daemon.emit("llm:stderr", json!("[embed] 預熱完成"));
    Ok(Json(json!({"ok": true, "url": base_url})))
}

async fn embedding_stop_handler(State(state): State<ApiState>) -> Json<Value> {
    state.daemon.embedding_user_stopped.store(true, Ordering::SeqCst);
    let mut guard = state.daemon.embedding_server.lock().await;
    if let Some(mut child) = guard.take() {
        child.kill().await.ok();
        child.wait().await.ok();
    }
    Json(json!({"ok": true}))
}

async fn embedding_restart_handler(State(state): State<ApiState>) -> Result<Json<Value>, (StatusCode, String)> {
    state.daemon.embedding_user_stopped.store(false, Ordering::SeqCst);
    {
        let mut guard = state.daemon.embedding_server.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill().await.ok();
            child.wait().await.ok();
        }
    }
    let base_url = ensure_embedding_running(&state).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    warmup_embedding_inference(&base_url).await;
    Ok(Json(json!({"ok": true})))
}

// ─── Inference route handlers ─────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct ChatRequest {
    system: String,
    user_content: String,
    #[serde(default = "default_max_tokens")]
    max_tokens: u32,
    #[serde(default = "default_temperature")]
    temperature: f32,
}
fn default_max_tokens() -> u32 { 1024 }
fn default_temperature() -> f32 { 0.3 }

async fn chat_handler(
    State(state): State<ApiState>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let base_url = ensure_llama_running(&state).await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e))?;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", base_url))
        .json(&json!({
            "messages": [
                {"role": "system", "content": req.system},
                {"role": "user",   "content": req.user_content},
            ],
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "stream": false,
        }))
        .timeout(Duration::from_secs(120))
        .send().await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("請求 llama-server 失敗：{}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err((StatusCode::BAD_GATEWAY, format!("llama-server 回應錯誤 {}：{}", status, text)));
    }

    let json: Value = resp.json().await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("解析回應失敗：{}", e)))?;

    let text = json["choices"][0]["message"]["content"]
        .as_str().unwrap_or("").trim().to_string();

    Ok(Json(json!({"text": text})))
}

#[derive(serde::Deserialize)]
struct SingleEmbeddingRequest {
    text: String,
}

async fn single_embedding_handler(
    State(state): State<ApiState>,
    Json(req): Json<SingleEmbeddingRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let base_url = ensure_embedding_running(&state).await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e))?;

    let client = reqwest::Client::new();
    let v = get_embedding_from_url(&client, &base_url, &req.text).await;
    Ok(Json(json!({"embedding": v})))
}

#[derive(serde::Deserialize)]
struct BatchEmbeddingRequest {
    texts: Vec<String>,
}

async fn batch_embedding_handler(
    State(state): State<ApiState>,
    Json(req): Json<BatchEmbeddingRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if req.texts.is_empty() {
        return Ok(Json(json!({"embeddings": []})));
    }

    let base_url = ensure_embedding_running(&state).await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e))?;
    let client = reqwest::Client::new();

    // Try batch first
    fn extract_batch(json: &Value, n: usize) -> Option<Vec<Vec<f32>>> {
        let data = json["data"].as_array()?;
        if data.is_empty() { return None; }
        let mut out: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
        for item in data {
            let idx = item["index"].as_u64().unwrap_or(0) as usize;
            if let Some(arr) = item["embedding"].as_array() {
                let v: Vec<f32> = arr.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
                if !v.is_empty() { out.push((idx, v)); }
            }
        }
        if out.is_empty() { return None; }
        out.sort_by_key(|(i, _)| *i);
        let mut result = vec![vec![]; n];
        for (idx, v) in out { if idx < n { result[idx] = v; } }
        Some(result)
    }

    let input: Vec<&str> = req.texts.iter().map(|s| s.as_str()).collect();
    for body in [
        json!({"input": &input}),
        json!({"input": &input, "model": "embedding"}),
    ] {
        if let Ok(resp) = client.post(format!("{}/v1/embeddings", base_url))
            .json(&body).timeout(Duration::from_secs(120)).send().await
        {
            if resp.status().is_success() {
                if let Ok(j) = resp.json::<Value>().await {
                    if let Some(batch) = extract_batch(&j, req.texts.len()) {
                        return Ok(Json(json!({"embeddings": batch})));
                    }
                }
            }
        }
    }

    // Fallback: individual
    let mut results = Vec::with_capacity(req.texts.len());
    for text in &req.texts {
        results.push(get_embedding_from_url(&client, &base_url, text).await);
    }
    Ok(Json(json!({"embeddings": results})))
}

// ─── Shutdown helpers ─────────────────────────────────────────────────────────

pub async fn kill_on_shutdown(state: &crate::daemon::state::DaemonState) {
    // Kill embedding first (depends on same binary as llama)
    {
        let mut guard = state.embedding_server.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill().await.ok();
            child.wait().await.ok();
        }
    }
    {
        let mut guard = state.llama_server.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill().await.ok();
            child.wait().await.ok();
        }
    }
}
