use axum::{routing::get, Json, Router};
use axum_server::tls_rustls::RustlsConfig;
use serde_json::json;
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

pub async fn run_https_server(
    cert_pem: String,
    key_pem: String,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let tls_config = RustlsConfig::from_pem(
        cert_pem.into_bytes(),
        key_pem.into_bytes(),
    )
    .await?;

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health_handler))
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("HTTPS server listening on https://0.0.0.0:{}", port);

    axum_server::bind_rustls(addr, tls_config)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

async fn health_handler() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "service": "notetreetlm-service",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
