mod auth;
mod daemon;
mod mdns;
mod server;
mod tls;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "notetreetlm_service=debug,info".into()),
        )
        .init();

    if let Err(e) = daemon::run().await {
        tracing::error!("Fatal error: {}", e);
        std::process::exit(1);
    }
}
