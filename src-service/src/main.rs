mod auth;
mod config;
mod daemon;
mod db;
pub mod service;
mod embedding;
mod routes;
mod app_state;

fn get_data_dir() -> std::path::PathBuf {
    let base = dirs::data_dir().expect("Cannot find data directory");
    base.join("com.notetreelm.app")
}

fn get_config_dir() -> std::path::PathBuf {
    let base = dirs::config_dir().expect("Cannot find config directory");
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

    let config_dir = get_config_dir();
    std::fs::create_dir_all(&config_dir).expect("Cannot create config dir");
    let config = config::Config::load_or_default(&config_dir);
    tracing::info!(
        "Config loaded — service=:{} llama=:{} embedding=:{} whisper=:{}",
        config.service.port,
        config.servers.llama_port,
        config.servers.embedding_port,
        config.servers.whisper_port,
    );

    if let Err(e) = daemon::run(data_dir, config).await {
        tracing::error!("Fatal error: {}", e);
        std::process::exit(1);
    }
}
