use tokio::signal;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("noteTreeLM Service starting...");

    // 1. 生成 TLS 憑證
    let hostnames = crate::tls::collect_san_hostnames();
    tracing::info!("TLS SAN hostnames: {:?}", hostnames);
    let tls = crate::tls::generate_tls_cert(hostnames)?;
    tracing::info!("TLS certificate generated, SPKI pin: {}", tls.spki_pin);

    // 2. 啟動 mDNS 廣播
    let _mdns = crate::mdns::start_mdns_broadcast(&tls.spki_pin)?;

    // 3. 啟動 HTTPS server（在背景 task 跑）
    let cert_pem = tls.cert_pem.clone();
    let key_pem = tls.key_pem.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::server::run_https_server(cert_pem, key_pem, 7788).await {
            tracing::error!("HTTPS server error: {}", e);
        }
    });

    tracing::info!("Service ready on https://0.0.0.0:7788");

    // 4. 等待 graceful shutdown
    shutdown_signal().await;
    tracing::info!("Service shutting down...");

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { tracing::info!("Received Ctrl+C"); },
        _ = terminate => { tracing::info!("Received SIGTERM"); },
    }
}
