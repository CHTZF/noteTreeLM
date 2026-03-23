use axum::{middleware, routing::{delete, get, post}, Json, Router};
use axum_server::tls_rustls::RustlsConfig;
use serde_json::json;
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use crate::auth::{handlers, middleware::auth_middleware, store::AuthStore};

pub async fn run_https_server(
    cert_pem: String,
    key_pem: String,
    port: u16,
    auth_store: AuthStore,
) -> Result<(), Box<dyn std::error::Error>> {
    let tls_config = RustlsConfig::from_pem(
        cert_pem.into_bytes(),
        key_pem.into_bytes(),
    ).await?;

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Public routes（不需要 token）
    let public_routes = Router::new()
        .route("/health", get(health_handler))
        .route("/auth/pair", post(handlers::pair))
        .route("/auth/refresh", post(handlers::refresh))
        .route("/auth/revoke", post(handlers::revoke))
        .with_state(auth_store.clone());

    // Protected routes（需要 token）
    let protected_routes = Router::new()
        .route("/auth/devices", get(handlers::list_devices))
        .route("/auth/devices/:id", delete(handlers::revoke_device))
        .layer(middleware::from_fn_with_state(auth_store.clone(), auth_middleware))
        .with_state(auth_store);

    let app = Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("HTTPS server listening on https://0.0.0.0:{}", port);

    axum_server::bind_rustls(addr, tls_config)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
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
