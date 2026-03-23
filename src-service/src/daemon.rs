use tokio::signal;
use crate::auth::store::AuthStore;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("noteTreeLM Service starting...");

    let hostnames = crate::tls::collect_san_hostnames();
    let tls = crate::tls::generate_tls_cert(hostnames)?;
    tracing::info!("TLS SPKI pin: {}", tls.spki_pin);

    let _mdns = crate::mdns::start_mdns_broadcast(&tls.spki_pin)?;

    let auth_store = AuthStore::new();

    let cert_pem = tls.cert_pem.clone();
    let key_pem = tls.key_pem.clone();
    let store_clone = auth_store.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::server::run_https_server(cert_pem, key_pem, 7788, store_clone).await {
            tracing::error!("HTTPS server error: {}", e);
        }
    });

    tracing::info!("Service ready on https://0.0.0.0:7788");
    shutdown_signal().await;
    tracing::info!("Service shutting down...");
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
