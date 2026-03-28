use axum::{
    body::Body,
    extract::State,
    http::Request,
    middleware,
    routing::{delete, get, post},
    Json, Router,
};
use axum_server::tls_rustls::RustlsConfig;
use serde_json::json;
use std::{net::SocketAddr, time::Duration};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::Span;
use crate::api_state::ApiState;
use crate::auth::{handlers, middleware::auth_middleware, store::AuthStore};
use crate::db::SurrealDb;
use crate::state::{DaemonState, ServerInfo};

/// Build the shared API router with all routes.
/// Used by both HTTP :7787 and HTTPS :7788.
pub fn build_api_router(app_state: ApiState) -> Router {
    let auth_store = app_state.auth.clone();

    // Public routes (no token required)
    let public_routes = Router::new()
        .route("/health", get(health_handler))
        .route("/auth/pair", post(handlers::pair))
        .route("/auth/refresh", post(handlers::refresh))
        .route("/auth/revoke", post(handlers::revoke))
        .with_state(auth_store.clone());

    // Device management routes (token required)
    let device_routes = Router::new()
        .route("/auth/devices", get(handlers::list_devices))
        .route("/auth/devices/:id", delete(handlers::revoke_device))
        .route("/auth/generate-pairing-code", post(handlers::generate_pairing_code))
        .layer(middleware::from_fn_with_state(auth_store.clone(), auth_middleware))
        .with_state(auth_store);

    // Server registry + admin routes
    let admin_routes = Router::new()
        .route("/servers/status", get(servers_status_handler))
        .route("/servers/register", post(servers_register_handler))
        .route("/db/repair", post(db_repair_handler))
        .with_state(app_state.clone());

    // API v1 routes (all the new domain routes)
    let api_v1 = Router::new()
        .merge(crate::routes::auth::router())
        .merge(crate::routes::settings::router())
        .merge(crate::routes::conversations::router())
        .merge(crate::routes::vault::router())
        .merge(crate::routes::notes::router())
        .merge(crate::routes::search::router())
        .merge(crate::routes::kb::router())
        .merge(crate::routes::agents::router())
        .merge(crate::routes::memory::router())
        .merge(crate::routes::scheduled::router())
        .merge(crate::routes::events::router())
        .route("/pairing-info", get(pairing_info_handler))
        .with_state(app_state);

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .nest("/api/v1", api_v1)
        .merge(public_routes)
        .merge(device_routes)
        .merge(admin_routes)
        .layer(cors)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &Request<Body>| {
                    tracing::info_span!(
                        "http_request",
                        method = %request.method(),
                        uri = %request.uri(),
                    )
                })
                .on_failure(|error: tower_http::classify::ServerErrorsFailureClass, latency: Duration, _span: &Span| {
                    tracing::error!("HTTP 500: {:?} latency={:?}", error, latency);
                })
        )
}

/// Plain HTTP server on localhost :7787 (no TLS)
pub async fn run_http_server(
    port: u16,
    app_state: ApiState,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = build_api_router(app_state);

    // Bind only to loopback — security: not accessible from outside
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    tracing::info!("HTTP server listening on http://127.0.0.1:{}", port);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(async move { let _ = shutdown_rx.recv().await; })
        .await?;

    Ok(())
}

/// HTTPS server on all interfaces :7788 (TLS, for mobile/external)
pub async fn run_https_server(
    cert_pem: String,
    key_pem: String,
    port: u16,
    auth_store: AuthStore,
    db: SurrealDb,
    daemon_state: DaemonState,
    handle: axum_server::Handle,
) -> Result<(), Box<dyn std::error::Error>> {
    let tls_config = RustlsConfig::from_pem(
        cert_pem.into_bytes(),
        key_pem.into_bytes(),
    ).await?;

    let app_state = ApiState::new(auth_store, db, daemon_state);
    let app = build_api_router(app_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("HTTPS server listening on https://0.0.0.0:{}", port);

    axum_server::bind_rustls(addr, tls_config)
        .handle(handle)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await?;

    Ok(())
}

async fn health_handler() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "service": "notetreelm-service",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn servers_status_handler(
    State(state): State<ApiState>,
) -> Json<serde_json::Value> {
    let servers = state.daemon.servers.read().await;
    Json(json!({ "servers": *servers }))
}

async fn servers_register_handler(
    State(state): State<ApiState>,
    Json(info): Json<ServerInfo>,
) -> Json<serde_json::Value> {
    // If an embedding server is registering, store its URL for vector search
    if info.name == "embedding" {
        let url = format!("http://127.0.0.1:{}", info.port);
        *state.daemon.embedding_url.write().await = Some(url);
    }
    if info.name == "llama" {
        let url = format!("http://127.0.0.1:{}", info.port);
        *state.daemon.llm_url.write().await = Some(url);
    }

    let mut servers = state.daemon.servers.write().await;
    if let Some(existing) = servers.iter_mut().find(|s| s.name == info.name) {
        *existing = info;
    } else {
        servers.push(info);
    }
    Json(json!({ "ok": true }))
}

async fn db_repair_handler(
    State(state): State<ApiState>,
) -> Json<serde_json::Value> {
    match crate::db::run_migrations_pub(&state.db).await {
        Ok(_) => Json(json!({ "ok": true, "message": "Migrations re-applied successfully" })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// Return tunnel URL and local URL for mobile pairing QR code.
/// Called by the desktop frontend; token is presented by the caller.
async fn pairing_info_handler(
    State(state): State<ApiState>,
) -> Json<serde_json::Value> {
    let tunnel_url = state.daemon.tunnel_url.read().await.clone();
    Json(json!({
        "tunnel_url": tunnel_url,
        "local_url": "http://127.0.0.1:7787",
    }))
}
