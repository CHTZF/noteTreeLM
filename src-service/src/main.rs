mod auth;
mod daemon;
mod db;
mod mdns;
mod server;
mod state;
mod tls;

fn get_data_dir() -> std::path::PathBuf {
    let base = dirs::data_dir().expect("Cannot find data directory");
    base.join("com.notetreetlm.app")
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "notetreetlm_service=debug,info".into()),
        )
        .init();

    let data_dir = get_data_dir();
    std::fs::create_dir_all(&data_dir).expect("Cannot create data dir");

    if let Err(e) = daemon::run(data_dir).await {
        tracing::error!("Fatal error: {}", e);
        std::process::exit(1);
    }
}
