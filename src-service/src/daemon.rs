use tokio::signal;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Daemon initialized");

    // TODO Phase 2: TLS + HTTP server
    // TODO Phase 3: Token auth
    // TODO Phase 4: SurrealDB
    // TODO Phase 5: AI servers + scheduler

    // Graceful shutdown：等待 SIGINT 或 SIGTERM
    shutdown_signal().await;
    tracing::info!("Daemon shutting down gracefully...");

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
