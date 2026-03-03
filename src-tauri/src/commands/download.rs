use crate::{db::queries, error::AppError, state::AppState};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex;

// ── Public event / response types ────────────────────────────────────────────

#[derive(Debug, Serialize, Clone)]
pub struct DownloadProgress {
    pub model_id: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub speed_bps: u64,
    /// "downloading" | "completed" | "error" | "cancelled"
    pub status: String,
    pub file_path: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ModelFileInfo {
    pub model_id: String,
    pub filename: String,
    pub file_path: String,
    pub size_bytes: u64,
    pub is_partial: bool,
}

// ── Managed download state ────────────────────────────────────────────────────

pub struct DownloadState {
    /// model_id → cancellation flag
    pub active: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl DownloadState {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(HashMap::new()),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn models_dir(app: &AppHandle) -> Result<std::path::PathBuf, AppError> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::Io(e.to_string()))?
        .join("models");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn emit_progress(app: &AppHandle, prog: DownloadProgress) {
    let _ = app.emit("model-download-progress", prog);
}

// ── Tauri commands ────────────────────────────────────────────────────────────

/// Returns the absolute path of the models directory.
#[tauri::command]
pub async fn get_models_dir(app: AppHandle) -> Result<String, AppError> {
    Ok(models_dir(&app)?.to_string_lossy().to_string())
}

/// Lists all files (complete and partial) in the models directory.
#[tauri::command]
pub async fn get_downloaded_models(app: AppHandle) -> Result<Vec<ModelFileInfo>, AppError> {
    let dir = models_dir(&app)?;
    let mut result = Vec::new();

    let read_dir = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return Ok(result),
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let is_partial = filename.ends_with(".part");
        let model_filename = if is_partial {
            filename.trim_end_matches(".part").to_string()
        } else {
            filename.clone()
        };

        let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let model_id = model_filename.trim_end_matches(".bin").to_string();

        result.push(ModelFileInfo {
            model_id,
            filename: model_filename,
            file_path: path.to_string_lossy().to_string(),
            size_bytes,
            is_partial,
        });
    }

    Ok(result)
}

/// Starts a download (or resumes from a .part file).
/// Returns immediately; progress is pushed via "model-download-progress" events.
#[tauri::command]
pub async fn start_model_download(
    app: AppHandle,
    dl_state: tauri::State<'_, DownloadState>,
    model_id: String,
    url: String,
    filename: String,
) -> Result<(), AppError> {
    // Security: prevent path traversal in filename
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return Err(AppError::Security("無效的檔案名稱".to_string()));
    }

    let dir = models_dir(&app)?;
    let final_path = dir.join(&filename);

    // Already complete
    if final_path.exists() {
        return Ok(());
    }

    // Prevent duplicate downloads
    let cancel_flag = {
        let mut active = dl_state.active.lock().await;
        if active.contains_key(&model_id) {
            return Ok(());
        }
        let flag = Arc::new(AtomicBool::new(false));
        active.insert(model_id.clone(), flag.clone());
        flag
    };

    // Resume offset from .part file
    let part_path = dir.join(format!("{}.part", filename));
    let resume_from = if part_path.exists() {
        std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };

    let app_clone = app.clone();
    let model_id_clone = model_id.clone();
    tokio::spawn(async move {
        let result = run_download(
            &app_clone,
            &model_id_clone,
            &url,
            &part_path,
            &final_path,
            resume_from,
            cancel_flag,
        )
        .await;

        // Cleanup active map
        if let Some(ds) = app_clone.try_state::<DownloadState>() {
            ds.active.lock().await.remove(&model_id_clone);
        }

        if let Err(e) = result {
            emit_progress(
                &app_clone,
                DownloadProgress {
                    model_id: model_id_clone,
                    downloaded_bytes: 0,
                    total_bytes: 0,
                    speed_bps: 0,
                    status: "error".to_string(),
                    file_path: None,
                    error: Some(e.to_string()),
                },
            );
        }
    });

    Ok(())
}

/// Sets the cancellation flag for an in-progress download.
#[tauri::command]
pub async fn cancel_model_download(
    dl_state: tauri::State<'_, DownloadState>,
    model_id: String,
) -> Result<(), AppError> {
    let active = dl_state.active.lock().await;
    if let Some(flag) = active.get(&model_id) {
        flag.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// Deletes a model file (complete or partial) from the models directory.
#[tauri::command]
pub async fn delete_model_file(app: AppHandle, filename: String) -> Result<(), AppError> {
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return Err(AppError::Security("無效的檔案名稱".to_string()));
    }
    let dir = models_dir(&app)?;
    // Remove complete file and/or part file
    for name in [filename.clone(), format!("{}.part", filename)] {
        let p = dir.join(&name);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
    }
    Ok(())
}

// ── External model path persistence ──────────────────────────────────────────

/// Returns the list of externally imported model paths for a given kind
/// ("whisper" | "llm"), stored in the settings DB under `external_models_{kind}`.
#[tauri::command]
pub async fn get_external_model_paths(
    state: tauri::State<'_, AppState>,
    kind: String,
) -> Result<Vec<String>, AppError> {
    let key = format!("external_models_{}", kind);
    let json = queries::get_setting(&state.settings_db, &key)
        .await?
        .unwrap_or_else(|| "[]".to_string());
    Ok(serde_json::from_str(&json).unwrap_or_default())
}

/// Persists the full list of externally imported model paths for a given kind.
#[tauri::command]
pub async fn set_external_model_paths(
    state: tauri::State<'_, AppState>,
    kind: String,
    paths: Vec<String>,
) -> Result<(), AppError> {
    let key = format!("external_models_{}", kind);
    let json = serde_json::to_string(&paths).map_err(|e| AppError::Io(e.to_string()))?;
    queries::set_setting(&state.settings_db, &key, &json).await?;
    Ok(())
}

// ── Binary (whisper-server) download ─────────────────────────────────────────

fn binaries_dir(app: &AppHandle) -> Result<std::path::PathBuf, AppError> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::Io(e.to_string()))?
        .join("bin");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Returns the absolute path to the downloaded whisper-server binary, or None if not installed.
#[tauri::command]
pub async fn get_whisper_binary_path(app: AppHandle) -> Result<Option<String>, AppError> {
    let path = binaries_dir(&app)?.join("whisper-server");
    if path.exists() {
        Ok(Some(path.to_string_lossy().into_owned()))
    } else {
        Ok(None)
    }
}

/// Downloads the latest whisper-server binary from GitHub Releases.
/// Returns immediately; progress is pushed via "model-download-progress" events
/// with model_id = "__whisper_server__".
#[tauri::command]
pub async fn download_whisper_server(
    app: AppHandle,
    dl_state: tauri::State<'_, DownloadState>,
) -> Result<(), AppError> {
    const MODEL_ID: &str = "__whisper_server__";

    // Prevent duplicate downloads
    {
        let mut active = dl_state.active.lock().await;
        if active.contains_key(MODEL_ID) {
            return Ok(());
        }
        active.insert(MODEL_ID.to_string(), Arc::new(AtomicBool::new(false)));
    }

    let app2 = app.clone();
    tokio::spawn(async move {
        let result = run_binary_download(&app2, MODEL_ID).await;

        if let Some(ds) = app2.try_state::<DownloadState>() {
            ds.active.lock().await.remove(MODEL_ID);
        }

        match result {
            Ok(path) => {
                emit_progress(&app2, DownloadProgress {
                    model_id: MODEL_ID.to_string(),
                    downloaded_bytes: 0,
                    total_bytes: 0,
                    speed_bps: 0,
                    status: "completed".to_string(),
                    file_path: Some(path),
                    error: None,
                });
            }
            Err(e) => {
                emit_progress(&app2, DownloadProgress {
                    model_id: MODEL_ID.to_string(),
                    downloaded_bytes: 0,
                    total_bytes: 0,
                    speed_bps: 0,
                    status: "error".to_string(),
                    file_path: None,
                    error: Some(e.to_string()),
                });
            }
        }
    });

    Ok(())
}

/// noteTreeLM 自己維護的預建 binary 所在 public repo
const OUR_REPO: &str = "CHTZF/noteTreeLM-releases";

/// 回傳當前平台對應的 asset 檔名（在 CHTZF/noteTreeLM-releases 上）
fn platform_asset_name() -> Result<&'static str, AppError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos",   "aarch64") => Ok("whisper-server-macos-arm64"),
        ("macos",   "x86_64")  => Ok("whisper-server-macos-x86_64"),
        ("windows", "x86_64")  => Ok("whisper-server-windows-x64.exe"),
        (os, arch) => Err(AppError::Import(format!(
            "此平台（{}/{}）尚不支援自動下載",
            os, arch
        ))),
    }
}

async fn run_binary_download(app: &AppHandle, model_id: &str) -> Result<String, AppError> {
    let asset_name = platform_asset_name()?;

    let client = reqwest::Client::builder()
        .user_agent("noteTreeLM/1.0")
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;

    match find_our_prebuilt(&client, asset_name).await {
        Ok(download_url) => download_single_binary(app, model_id, &client, &download_url).await,
        Err(_) => Err(AppError::Import(
            "找不到預建版本。請確認網路連線，或至 GitHub → Actions → \
             「Build whisper-server」手動執行 workflow 後再試。\n\
             若有網路限制，可自行編譯：cmake … -DWHISPER_BUILD_SERVER=ON \
             -DGGML_METAL=ON -DGGML_METAL_EMBED_LIBRARY=ON"
                .to_string(),
        )),
    }
}

/// 在 CHTZF/noteTreeLM-releases 中尋找最新含有目標 asset 的 whisper-* release
async fn find_our_prebuilt(
    client: &reqwest::Client,
    asset_name: &str,
) -> Result<String, AppError> {
    let releases: Vec<serde_json::Value> = client
        .get(format!(
            "https://api.github.com/repos/{}/releases?per_page=20",
            OUR_REPO
        ))
        .send()
        .await
        .map_err(|e| AppError::Import(e.to_string()))?
        .json()
        .await
        .map_err(|e| AppError::Import(e.to_string()))?;

    for release in &releases {
        let tag = release["tag_name"].as_str().unwrap_or("");
        if !tag.starts_with("whisper-") {
            continue;
        }
        if let Some(assets) = release["assets"].as_array() {
            for asset in assets {
                if asset["name"].as_str() == Some(asset_name) {
                    if let Some(url) = asset["browser_download_url"].as_str() {
                        return Ok(url.to_string());
                    }
                }
            }
        }
    }
    Err(AppError::Import("找不到預建版本".to_string()))
}

/// 直接下載單一 binary 檔案（非 zip），儲存到 bin_dir/whisper-server
async fn download_single_binary(
    app: &AppHandle,
    model_id: &str,
    client: &reqwest::Client,
    url: &str,
) -> Result<String, AppError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| AppError::Import(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(AppError::Import(format!("下載失敗 HTTP {}", resp.status())));
    }

    let total_bytes = resp.content_length().unwrap_or(0);
    let bin_dir = binaries_dir(app)?;
    let dest = bin_dir.join("whisper-server");
    let part = bin_dir.join("whisper-server.part");

    let mut file = std::fs::File::create(&part)?;
    let mut downloaded = 0u64;
    let mut last_emit = std::time::Instant::now();
    let mut last_emit_downloaded = 0u64;
    let mut resp = resp;

    loop {
        match resp.chunk().await.map_err(|e| AppError::Import(e.to_string()))? {
            None => break,
            Some(chunk) => {
                file.write_all(&chunk)?;
                downloaded += chunk.len() as u64;
                let elapsed = last_emit.elapsed();
                if elapsed >= std::time::Duration::from_millis(300) {
                    let speed_bps = if elapsed.as_secs_f64() > 0.0 {
                        ((downloaded - last_emit_downloaded) as f64 / elapsed.as_secs_f64()) as u64
                    } else { 0 };
                    last_emit = std::time::Instant::now();
                    last_emit_downloaded = downloaded;
                    emit_progress(app, DownloadProgress {
                        model_id: model_id.to_string(),
                        downloaded_bytes: downloaded,
                        total_bytes: total_bytes,
                        speed_bps,
                        status: "downloading".to_string(),
                        file_path: None,
                        error: None,
                    });
                }
            }
        }
    }
    file.flush()?;
    drop(file);

    std::fs::rename(&part, &dest)?;

    // chmod +x (macOS / Linux)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
    }

    Ok(dest.to_string_lossy().into_owned())
}


// ── Binary (llama-server) download ───────────────────────────────────────────

/// Returns the absolute path to the downloaded llama-server binary, or None if not installed.
#[tauri::command]
pub async fn get_llama_binary_path(app: AppHandle) -> Result<Option<String>, AppError> {
    let name = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
    let path = binaries_dir(&app)?.join(name);
    if path.exists() {
        Ok(Some(path.to_string_lossy().into_owned()))
    } else {
        Ok(None)
    }
}

/// Downloads the latest llama-server binary from ggerganov/llama.cpp GitHub Releases.
/// Returns immediately; progress is pushed via "model-download-progress" events
/// with model_id = "__llama_server__".
#[tauri::command]
pub async fn download_llama_server(
    app: AppHandle,
    dl_state: tauri::State<'_, DownloadState>,
) -> Result<(), AppError> {
    const MODEL_ID: &str = "__llama_server__";

    {
        let mut active = dl_state.active.lock().await;
        if active.contains_key(MODEL_ID) {
            return Ok(());
        }
        active.insert(MODEL_ID.to_string(), Arc::new(AtomicBool::new(false)));
    }

    let app2 = app.clone();
    tokio::spawn(async move {
        let result = run_llama_download(&app2, MODEL_ID).await;

        if let Some(ds) = app2.try_state::<DownloadState>() {
            ds.active.lock().await.remove(MODEL_ID);
        }

        match result {
            Ok(path) => {
                emit_progress(&app2, DownloadProgress {
                    model_id: MODEL_ID.to_string(),
                    downloaded_bytes: 0,
                    total_bytes: 0,
                    speed_bps: 0,
                    status: "completed".to_string(),
                    file_path: Some(path),
                    error: None,
                });
            }
            Err(e) => {
                emit_progress(&app2, DownloadProgress {
                    model_id: MODEL_ID.to_string(),
                    downloaded_bytes: 0,
                    total_bytes: 0,
                    speed_bps: 0,
                    status: "error".to_string(),
                    file_path: None,
                    error: Some(e.to_string()),
                });
            }
        }
    });

    Ok(())
}

/// 回傳當前平台對應的 (asset 關鍵字, 壓縮格式副檔名, binary 名稱)
fn llama_platform_pattern() -> Result<(&'static str, &'static str, &'static str), AppError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos",   "aarch64") => Ok(("macos-arm64", ".tar.gz", "llama-server")),
        ("macos",   "x86_64")  => Ok(("macos-x64",   ".tar.gz", "llama-server")),
        ("windows", "x86_64")  => Ok(("win-cpu-x64",  ".zip",   "llama-server.exe")),
        (os, arch) => Err(AppError::Import(format!(
            "此平台（{}/{}）尚不支援自動下載", os, arch
        ))),
    }
}

async fn run_llama_download(app: &AppHandle, model_id: &str) -> Result<String, AppError> {
    let (asset_pattern, file_ext, binary_name) = llama_platform_pattern()?;

    let client = reqwest::Client::builder()
        .user_agent("noteTreeLM/1.0")
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;

    // 查詢 ggerganov/llama.cpp 最新 release
    let release: serde_json::Value = client
        .get("https://api.github.com/repos/ggerganov/llama.cpp/releases/latest")
        .send().await
        .map_err(|e| AppError::Import(format!("無法連接 GitHub API：{}", e)))?
        .json().await
        .map_err(|e| AppError::Import(e.to_string()))?;

    let assets = release["assets"].as_array()
        .ok_or_else(|| AppError::Import("GitHub release 格式異常".to_string()))?;

    // 選出符合平台的 archive（macOS: .tar.gz，Windows: .zip）
    let download_url = assets.iter()
        .find_map(|a| {
            let name = a["name"].as_str()?.to_lowercase();
            let url  = a["browser_download_url"].as_str()?;
            if name.contains(asset_pattern) && name.ends_with(file_ext) {
                Some(url.to_string())
            } else {
                None
            }
        })
        .ok_or_else(|| AppError::Import(format!(
            "找不到適合 {} 平台的 llama-server 下載包",
            asset_pattern
        )))?;

    download_and_extract_llama(app, model_id, &client, &download_url, file_ext, binary_name).await
}

/// 下載壓縮包（zip 或 tar.gz），解壓出 llama-server binary，儲存到 bin_dir
async fn download_and_extract_llama(
    app: &AppHandle,
    model_id: &str,
    client: &reqwest::Client,
    url: &str,
    file_ext: &str,
    binary_name: &str,
) -> Result<String, AppError> {
    let resp = client.get(url).send().await
        .map_err(|e| AppError::Import(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(AppError::Import(format!("下載失敗 HTTP {}", resp.status())));
    }

    let total_bytes = resp.content_length().unwrap_or(0);
    let bin_dir = binaries_dir(app)?;
    let archive_part = bin_dir.join(format!("llama-server{}.part", file_ext));

    let mut file = std::fs::File::create(&archive_part)?;
    let mut downloaded = 0u64;
    let mut last_emit = std::time::Instant::now();
    let mut last_emit_downloaded = 0u64;
    let mut resp = resp;

    loop {
        match resp.chunk().await.map_err(|e| AppError::Import(e.to_string()))? {
            None => break,
            Some(chunk) => {
                file.write_all(&chunk)?;
                downloaded += chunk.len() as u64;
                let elapsed = last_emit.elapsed();
                if elapsed >= std::time::Duration::from_millis(300) {
                    let speed_bps = if elapsed.as_secs_f64() > 0.0 {
                        ((downloaded - last_emit_downloaded) as f64 / elapsed.as_secs_f64()) as u64
                    } else { 0 };
                    last_emit = std::time::Instant::now();
                    last_emit_downloaded = downloaded;
                    emit_progress(app, DownloadProgress {
                        model_id: model_id.to_string(),
                        downloaded_bytes: downloaded,
                        total_bytes: total_bytes,
                        speed_bps,
                        status: "downloading".to_string(),
                        file_path: None,
                        error: None,
                    });
                }
            }
        }
    }
    file.flush()?;
    drop(file);

    let dest = bin_dir.join(binary_name);
    let archive_bytes = std::fs::read(&archive_part)
        .map_err(|e| AppError::Import(e.to_string()))?;

    if file_ext == ".tar.gz" {
        // macOS / Linux：tar.gz 解壓
        use flate2::read::GzDecoder;
        use tar::Archive;
        let gz = GzDecoder::new(std::io::Cursor::new(archive_bytes));
        let mut tar = Archive::new(gz);
        let mut found = false;
        for entry in tar.entries().map_err(|e| AppError::Import(e.to_string()))? {
            let mut entry = entry.map_err(|e| AppError::Import(e.to_string()))?;
            let path = entry.path().map_err(|e| AppError::Import(e.to_string()))?.to_string_lossy().to_string();
            let file_part = path.rsplit('/').next().unwrap_or(&path).to_lowercase();
            if file_part == binary_name.to_lowercase() {
                entry.unpack(&dest).map_err(|e| AppError::Import(format!("解壓失敗：{}", e)))?;
                found = true;
                break;
            }
        }
        if !found {
            return Err(AppError::Import(format!("tar.gz 中找不到 {}", binary_name)));
        }
    } else {
        // Windows：zip 解壓
        let cursor = std::io::Cursor::new(archive_bytes);
        let mut archive = zip::ZipArchive::new(cursor)
            .map_err(|e| AppError::Import(format!("zip 解壓失敗：{}", e)))?;

        let entry_name = (0..archive.len())
            .find_map(|i| {
                let e = archive.by_index(i).ok()?;
                let name = e.name().to_string();
                let file_part = name.rsplit('/').next().unwrap_or(&name).to_lowercase();
                if file_part == binary_name.to_lowercase() { Some(name) } else { None }
            })
            .ok_or_else(|| AppError::Import(format!("zip 中找不到 {}", binary_name)))?;

        let mut entry = archive.by_name(&entry_name)
            .map_err(|e| AppError::Import(e.to_string()))?;
        let mut out = std::fs::File::create(&dest)?;
        std::io::copy(&mut entry, &mut out)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
    }

    let _ = std::fs::remove_file(&archive_part);
    Ok(dest.to_string_lossy().into_owned())
}

// ── Core download logic ───────────────────────────────────────────────────────

async fn run_download(
    app: &AppHandle,
    model_id: &str,
    url: &str,
    part_path: &std::path::Path,
    final_path: &std::path::Path,
    resume_from: u64,
    cancel_flag: Arc<AtomicBool>,
) -> Result<(), AppError> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;

    let mut req = client.get(url);
    if resume_from > 0 {
        req = req.header("Range", format!("bytes={}-", resume_from));
    }

    let resp = req.send().await?;
    let http_status = resp.status();
    if !http_status.is_success() && http_status.as_u16() != 206 {
        return Err(AppError::Import(format!("HTTP {} 下載失敗", http_status)));
    }

    let content_length = resp.content_length().unwrap_or(0);
    let total_bytes = content_length + resume_from;

    // Open or create the .part file
    let mut file = if resume_from > 0 {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(part_path)?
    } else {
        std::fs::File::create(part_path)?
    };

    emit_progress(
        app,
        DownloadProgress {
            model_id: model_id.to_string(),
            downloaded_bytes: resume_from,
            total_bytes,
            speed_bps: 0,
            status: "downloading".to_string(),
            file_path: None,
            error: None,
        },
    );

    let mut downloaded = resume_from;
    let mut last_emit = std::time::Instant::now();
    let mut bytes_since_last = 0u64;
    let mut resp = resp;

    loop {
        // Poll cancellation flag (lock-free)
        if cancel_flag.load(Ordering::Relaxed) {
            emit_progress(
                app,
                DownloadProgress {
                    model_id: model_id.to_string(),
                    downloaded_bytes: downloaded,
                    total_bytes,
                    speed_bps: 0,
                    status: "cancelled".to_string(),
                    file_path: None,
                    error: None,
                },
            );
            return Ok(());
        }

        match resp.chunk().await? {
            None => break, // Stream exhausted
            Some(chunk) => {
                file.write_all(&chunk)?;
                let len = chunk.len() as u64;
                downloaded += len;
                bytes_since_last += len;

                let elapsed = last_emit.elapsed();
                if elapsed >= std::time::Duration::from_millis(300) {
                    let speed = (bytes_since_last as f64 / elapsed.as_secs_f64()) as u64;
                    bytes_since_last = 0;
                    last_emit = std::time::Instant::now();

                    emit_progress(
                        app,
                        DownloadProgress {
                            model_id: model_id.to_string(),
                            downloaded_bytes: downloaded,
                            total_bytes,
                            speed_bps: speed,
                            status: "downloading".to_string(),
                            file_path: None,
                            error: None,
                        },
                    );
                }
            }
        }
    }

    // Flush and promote .part → final
    file.flush()?;
    drop(file);
    std::fs::rename(part_path, final_path)?;

    emit_progress(
        app,
        DownloadProgress {
            model_id: model_id.to_string(),
            downloaded_bytes: downloaded,
            total_bytes: downloaded,
            speed_bps: 0,
            status: "completed".to_string(),
            file_path: Some(final_path.to_string_lossy().to_string()),
            error: None,
        },
    );

    Ok(())
}
