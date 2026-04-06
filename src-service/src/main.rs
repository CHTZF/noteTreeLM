mod auth;
pub(crate) mod crypto;
mod daemon;
mod db;
pub mod service;
mod network;
mod processing;
mod routes;
mod server;
mod app_state;

fn get_data_dir() -> std::path::PathBuf {
    let base = dirs::data_dir().expect("Cannot find data directory");
    base.join("com.notetreelm.app")
}

#[tokio::main]
async fn main() {
    // rustls 0.23 requires an explicit crypto provider; install ring before any TLS usage
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "notetreelm_service=debug,info".into()),
        )
        .init();

    let data_dir = get_data_dir();
    std::fs::create_dir_all(&data_dir).expect("Cannot create data dir");

    if let Err(e) = daemon::run(data_dir).await {
        tracing::error!("Fatal error: {}", e);
        std::process::exit(1);
    }
}
