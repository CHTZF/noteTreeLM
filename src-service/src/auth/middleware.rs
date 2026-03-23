use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use super::store::AuthStore;

pub async fn auth_middleware(
    State(store): State<AuthStore>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // localhost 豁免
    if let Some(addr) = request.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>() {
        if addr.ip().is_loopback() {
            return Ok(next.run(request).await);
        }
    }

    // 讀取 Authorization header
    let token = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    store.verify_access_token(token).await
        .ok_or(StatusCode::UNAUTHORIZED)?;

    Ok(next.run(request).await)
}
