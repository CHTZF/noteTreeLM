mod scheduler;
mod cloudflared;
pub(crate) mod state;

use std::path::PathBuf;
use tokio::signal;
use crate::app_state::ApiState;
use crate::auth::store::AuthStore;
use crate::daemon::state::DaemonState;

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
            cloudflared::spawn(tunnel_url_ref).await;
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

    // Wait for both servers + scheduler to drain, up to 15 seconds
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        async { let _ = tokio::join!(http_task, https_task, sched_task); },
    ).await;

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
