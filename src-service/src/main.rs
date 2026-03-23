mod daemon;

#[tokio::main]
async fn main() {
    // 初始化 tracing logger
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "notetreetlm_service=debug,info".into()),
        )
        .init();

    tracing::info!("noteTreeLM Service starting...");

    if let Err(e) = daemon::run().await {
        tracing::error!("Service error: {}", e);
        std::process::exit(1);
    }
}
