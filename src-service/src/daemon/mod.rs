mod scheduler;
mod cloudflared;
pub(crate) mod state;
pub(crate) mod server;
pub(crate) mod network;

use std::path::PathBuf;
use tokio::signal;
use crate::app_state::ApiState;
use crate::auth::store::AuthStore;
use crate::config::Config;
use crate::daemon::state::DaemonState;

pub async fn run(data_dir: PathBuf, config: Config) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("noteTreeLM Service starting...");

    let db = crate::db::init_db(&data_dir).await?;
    tracing::info!("Database initialized");

    let hostnames = network::tls::collect_san_hostnames();
    let tls = network::tls::generate_tls_cert(hostnames)?;
    tracing::info!("TLS SPKI pin: {}", tls.spki_pin);

    let _mdns = network::mdns::start_mdns_broadcast(&tls.spki_pin)?;

    let auth_store = AuthStore::new();
    let daemon_state = DaemonState::new(&data_dir, &config);

    // Load diarize model if path is configured in settings
    match crate::diarize::load_from_db(&db).await {
        Ok(Some(model)) => {
            tracing::info!("Diarize model loaded successfully");
            if let Ok(mut guard) = daemon_state.diarize_model.write() {
                *guard = Some(model);
            }
        }
        Ok(None) => tracing::info!("Diarize model path not configured — speaker diarization disabled"),
        Err(e)   => tracing::warn!("Diarize model failed to load: {}", e),
    }

    let http_port = config.service.port;
    let https_port = config.service.https_port;

    // Graceful shutdown coordination
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let https_handle = axum_server::Handle::new();

    // Spawn scheduler loop (given its own shutdown receiver so it exits cleanly)
    let sched_task = {
        let sched_api_state = ApiState::new(auth_store.clone(), db.clone(), daemon_state.clone());
        let sched_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            scheduler::run(sched_api_state, sched_rx).await;
        })
    };

    // HTTP localhost server (no TLS) on :7787
    let http_task = {
        let api_state = ApiState::new(auth_store.clone(), db.clone(), daemon_state.clone());
        let rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            if let Err(e) = server::run_http_server(http_port, api_state, rx).await {
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
            if let Err(e) = server::run_https_server(
                cert_pem, key_pem, https_port, store_clone, db_clone, daemon_state_clone, handle,
            ).await {
                tracing::error!("HTTPS server error: {}", e);
            }
        })
    };

    // Spawn cloudflared tunnel (best-effort; silently skipped if binary not found)
    {
        let tunnel_url_ref = daemon_state.tunnel_url.clone();
        tokio::spawn(async move {
            cloudflared::spawn(tunnel_url_ref).await;
        });
    }

    tracing::info!("Service ready — HTTP http://127.0.0.1:{} | HTTPS https://0.0.0.0:{}", http_port, https_port);
    shutdown_signal().await;
    tracing::info!("Service shutting down gracefully — waiting for in-flight requests...");

    // Signal both servers to stop accepting new connections.
    // In-flight requests (e.g. scan_vault) are allowed to complete so that
    // SurrealDB transactions are committed rather than dropped mid-flight.
    let _ = shutdown_tx.send(());
    https_handle.graceful_shutdown(Some(std::time::Duration::from_secs(15)));

    // Wait for both servers + scheduler to drain, up to 15 seconds
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        async { let _ = tokio::join!(http_task, https_task, sched_task); },
    ).await;

    // Kill all managed server processes (service owns their lifecycle)
    crate::routes::whisper::kill_on_shutdown(&daemon_state).await;
    crate::routes::llm::kill_on_shutdown(&daemon_state).await;

    // Drop db last — all tasks that hold Arc<db> clones must have finished first
    // so that no in-flight transactions are abandoned when the connection closes.
    drop(db);

    tracing::info!("Service shutdown complete");
    Ok(())
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
