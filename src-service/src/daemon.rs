use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::signal;
use tokio::sync::RwLock;
use crate::api_state::ApiState;
use crate::auth::store::AuthStore;
use crate::state::DaemonState;

pub async fn run(data_dir: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("noteTreeLM Service starting...");

    let db = crate::db::init_db(&data_dir).await?;
    tracing::info!("Database initialized");

    let hostnames = crate::network::tls::collect_san_hostnames();
    let tls = crate::network::tls::generate_tls_cert(hostnames)?;
    tracing::info!("TLS SPKI pin: {}", tls.spki_pin);

    let _mdns = crate::network::mdns::start_mdns_broadcast(&tls.spki_pin)?;

    let auth_store = AuthStore::new();
    let daemon_state = DaemonState::new_with_data_dir(&data_dir);

    // Graceful shutdown coordination
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let https_handle = axum_server::Handle::new();

    // Spawn scheduler loop
    {
        let sched_api_state = ApiState::new(auth_store.clone(), db.clone(), daemon_state.clone());
        tokio::spawn(async move {
            run_scheduler(sched_api_state).await;
        });
    }

    // HTTP localhost server (no TLS) on :7787
    let http_task = {
        let api_state = ApiState::new(auth_store.clone(), db.clone(), daemon_state.clone());
        let rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            if let Err(e) = crate::server::run_http_server(7787, api_state, rx).await {
                tracing::error!("HTTP server error: {}", e);
            }
        })
    };

    // HTTPS external server (TLS) on :7788
    let https_task = {
        let cert_pem = tls.cert_pem.clone();
        let key_pem = tls.key_pem.clone();
        let store_clone = auth_store.clone();
        let db_clone = db.clone();
        let daemon_state_clone = daemon_state.clone();
        let handle = https_handle.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::server::run_https_server(
                cert_pem, key_pem, 7788, store_clone, db_clone, daemon_state_clone, handle,
            ).await {
                tracing::error!("HTTPS server error: {}", e);
            }
        })
    };

    // Spawn cloudflared tunnel (best-effort; silently skipped if binary not found)
    {
        let tunnel_url_ref = daemon_state.tunnel_url.clone();
        tokio::spawn(async move {
            spawn_cloudflared(tunnel_url_ref).await;
        });
    }

    tracing::info!("Service ready — HTTP http://127.0.0.1:7787 | HTTPS https://0.0.0.0:7788");
    shutdown_signal().await;
    tracing::info!("Service shutting down gracefully — waiting for in-flight requests...");

    // Signal both servers to stop accepting new connections.
    // In-flight requests (e.g. scan_vault) are allowed to complete so that
    // SurrealDB transactions are committed rather than dropped mid-flight.
    let _ = shutdown_tx.send(());
    https_handle.graceful_shutdown(Some(std::time::Duration::from_secs(15)));

    // Wait for both servers to drain, up to 15 seconds
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        async { let _ = tokio::join!(http_task, https_task); },
    ).await;

    tracing::info!("Service shutdown complete");
    Ok(())
}

async fn run_scheduler(state: crate::api_state::ApiState) {
    tracing::info!("Scheduler started");
    // Track last cleanup to run once per day
    let mut last_cleanup_ts: i64 = 0;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

        let now_ts = chrono::Utc::now().timestamp();

        // Daily cleanup: delete expired memory_facts (runs at most once per 24h)
        if now_ts - last_cleanup_ts >= 86400 {
            match state.db
                .query("DELETE memory_facts WHERE expires_at < $now")
                .bind(("now", now_ts))
                .await
            {
                Ok(_) => tracing::info!("[scheduler] expired memory_facts cleanup done"),
                Err(e) => tracing::warn!("[scheduler] cleanup error: {}", e),
            }
            last_cleanup_ts = now_ts;
        }

        #[derive(serde::Deserialize)]
        struct TaskRow {
            task_id: String,
            vault_id: String,
            account_id: Option<String>,
            description: String,
            agent_def_name: Option<String>,
            agent_prompt: Option<String>,
            repeat_interval_secs: i64,
        }

        let mut resp = match state.db.query(
            "SELECT task_id, vault_id, account_id, description, agent_def_name, agent_prompt, repeat_interval_secs \
             FROM scheduled_tasks \
             WHERE status = 'pending' AND run_at_ts <= $now"
        )
        .bind(("now", now_ts))
        .await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Scheduler query error: {}", e);
                continue;
            }
        };

        let due: Vec<TaskRow> = resp.take(0).unwrap_or_default();
        for task in due {
            tracing::info!(
                "Scheduled task due: {} (vault={}, agent={:?})",
                task.description, task.vault_id, task.agent_def_name
            );

            // Emit SSE notification to frontend
            state.daemon.emit("schedule:triggered", serde_json::json!({
                "task_id": task.task_id,
                "vault_id": task.vault_id,
                "description": task.description,
            }));

            // Execute agent in background
            let state_clone = state.clone();
            let tid = task.task_id.clone();
            let vid = task.vault_id.clone();
            let aid = task.account_id.clone().unwrap_or_default();
            let desc = task.description.clone();
            let agent_name = task.agent_def_name.clone();
            let agent_prompt = task.agent_prompt.clone();
            tokio::spawn(async move {
                crate::service_agent::execute_scheduled_task(
                    state_clone, tid, vid, aid, agent_name, agent_prompt, desc,
                ).await;
            });

            // Update schedule
            if task.repeat_interval_secs > 0 {
                let next_ts = now_ts + task.repeat_interval_secs;
                let _ = state.db.query(
                    "UPDATE scheduled_tasks SET run_at_ts = $next WHERE task_id = $tid"
                )
                .bind(("next", next_ts))
                .bind(("tid", task.task_id.clone()))
                .await;
            } else {
                let _ = state.db.query(
                    "UPDATE scheduled_tasks SET status = 'done' WHERE task_id = $tid"
                )
                .bind(("tid", task.task_id.clone()))
                .await;
            }
        }
    }
}

/// Platform-specific cloudflared binary filename and download info.
struct CloudflaredRelease {
    /// URL to download
    url: &'static str,
    /// Whether the download is a .tgz that needs extracting (macOS/Linux tarballs)
    is_tgz: bool,
}

fn cloudflared_release() -> Option<CloudflaredRelease> {
    // macOS: distributed as .tgz
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Some(CloudflaredRelease {
        url: "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-darwin-arm64.tgz",
        is_tgz: true,
    });
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return Some(CloudflaredRelease {
        url: "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-darwin-amd64.tgz",
        is_tgz: true,
    });

    // Windows: distributed as bare .exe
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return Some(CloudflaredRelease {
        url: "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-windows-amd64.exe",
        is_tgz: false,
    });
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    return Some(CloudflaredRelease {
        url: "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-windows-arm64.exe",
        is_tgz: false,
    });

    // Linux: distributed as bare binary
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return Some(CloudflaredRelease {
        url: "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64",
        is_tgz: false,
    });
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return Some(CloudflaredRelease {
        url: "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-arm64",
        is_tgz: false,
    });

    #[allow(unreachable_code)]
    None
}

/// Binary filename in the cache dir (platform-aware).
fn cloudflared_cache_path() -> PathBuf {
    let base = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::temp_dir()))
        .join("notetreelm");

    #[cfg(target_os = "windows")]
    return base.join("cloudflared.exe");
    #[cfg(not(target_os = "windows"))]
    return base.join("cloudflared");
}

/// Locate or auto-download cloudflared, returning the path to the binary.
async fn ensure_cloudflared() -> Option<PathBuf> {
    // 1. Explicit env var override
    if let Ok(p) = std::env::var("CLOUDFLARED_PATH") {
        let pb = PathBuf::from(p);
        if pb.exists() { return Some(pb); }
    }

    // 2. Common Unix system install paths (Homebrew, system package managers)
    #[cfg(not(target_os = "windows"))]
    {
        let candidates = [
            "/opt/homebrew/bin/cloudflared",  // Apple Silicon Homebrew
            "/usr/local/bin/cloudflared",     // Intel Homebrew / manual
            "/usr/bin/cloudflared",
        ];
        if let Some(p) = candidates.iter().find(|p| std::path::Path::new(p).exists()) {
            return Some(PathBuf::from(p));
        }
    }

    // 3. App data cache (previously downloaded)
    let cache_path = cloudflared_cache_path();
    if cache_path.exists() {
        return Some(cache_path);
    }

    // 4. Auto-download from Cloudflare GitHub releases
    let release = match cloudflared_release() {
        Some(r) => r,
        None => {
            tracing::warn!("cloudflared auto-download not supported on this platform/arch");
            return None;
        }
    };

    tracing::info!("cloudflared not found, downloading from {}…", release.url);
    let bytes = match reqwest::get(release.url).await {
        Ok(resp) => match resp.bytes().await {
            Ok(b) => b,
            Err(e) => { tracing::warn!("cloudflared download failed: {}", e); return None; }
        },
        Err(e) => { tracing::warn!("cloudflared download failed: {}", e); return None; }
    };

    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if release.is_tgz {
        // Write .tgz to temp file and extract with system tar
        let tmp_tgz = cache_path.with_extension("tgz");
        if let Err(e) = std::fs::write(&tmp_tgz, &bytes) {
            tracing::warn!("cloudflared tgz write failed: {}", e);
            return None;
        }
        let extract_dir = cache_path.parent().unwrap();
        let status = tokio::process::Command::new("tar")
            .args(["xzf", tmp_tgz.to_str().unwrap_or(""), "-C", extract_dir.to_str().unwrap_or("")])
            .status().await;
        let _ = std::fs::remove_file(&tmp_tgz);
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => { tracing::warn!("cloudflared tar extract failed: exit {}", s); return None; }
            Err(e) => { tracing::warn!("cloudflared tar extract failed: {}", e); return None; }
        }
        // tar extracts "cloudflared" into extract_dir; rename if needed
        let extracted = extract_dir.join("cloudflared");
        if extracted != cache_path {
            if let Err(e) = std::fs::rename(&extracted, &cache_path) {
                tracing::warn!("cloudflared rename failed: {}", e);
                return None;
            }
        }
    } else {
        // Bare binary (Windows .exe or Linux)
        if let Err(e) = std::fs::write(&cache_path, &bytes) {
            tracing::warn!("cloudflared write failed: {}", e);
            return None;
        }
    }

    // Set executable bit on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&cache_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            let _ = std::fs::set_permissions(&cache_path, perms);
        }
    }

    tracing::info!("cloudflared downloaded to {}", cache_path.display());
    Some(cache_path)
}

/// Attempt to spawn cloudflared and parse the public tunnel URL from its output.
/// Auto-downloads cloudflared if not found. Runs forever (cloudflared is long-lived).
async fn spawn_cloudflared(tunnel_url: Arc<RwLock<Option<String>>>) {
    let binary = match ensure_cloudflared().await {
        Some(p) => p,
        None => {
            tracing::info!("cloudflared unavailable, tunnel disabled");
            return;
        }
    };

    let mut cmd = tokio::process::Command::new(&binary);
    cmd.args(["tunnel", "--url", "http://127.0.0.1:7787"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::info!("cloudflared not available ({}), tunnel disabled", e);
            return;
        }
    };

    tracing::info!("cloudflared spawned, waiting for tunnel URL...");

    // Drain stderr to prevent SIGPIPE; parse tunnel URL from output.
    // Must keep reading until cloudflared exits — breaking closes the pipe
    // which sends SIGPIPE and kills cloudflared.
    if let Some(stderr) = child.stderr.take() {
        let reader = tokio::io::BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!("cloudflared: {}", line);
            if tunnel_url.read().await.is_none() {
                if let Some(url) = extract_tunnel_url(&line) {
                    tracing::info!("Cloudflare Tunnel URL: {}", url);
                    *tunnel_url.write().await = Some(url);
                }
            }
        }
    }

    // Keep process alive
    let _ = child.wait().await;
    tracing::info!("cloudflared exited");
    *tunnel_url.write().await = None;
}

fn extract_tunnel_url(line: &str) -> Option<String> {
    // Match https://something.trycloudflare.com
    let marker = "https://";
    let suffix = ".trycloudflare.com";
    let start = line.find(marker)?;
    let rest = &line[start..];
    let end = rest.find(|c: char| c.is_whitespace() || c == '|' || c == '"').unwrap_or(rest.len());
    let candidate = &rest[..end];
    if candidate.contains(suffix) {
        Some(candidate.to_string())
    } else {
        None
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("Ctrl+C handler failed");
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler failed")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
