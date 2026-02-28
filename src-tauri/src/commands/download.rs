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
    let json = queries::get_setting(&state.db, &key)
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
    queries::set_setting(&state.db, &key, &json).await?;
    Ok(())
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
