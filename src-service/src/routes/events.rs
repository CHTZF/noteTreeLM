use axum::{
    extract::State,
    response::{sse::Event, IntoResponse, Sse},
};
use futures_util::stream::StreamExt;
use std::convert::Infallible;
use tokio_stream::wrappers::BroadcastStream;

use crate::app_state::ApiState;

pub fn router() -> axum::Router<ApiState> {
    axum::Router::new().route("/events", axum::routing::get(sse_handler))
}

/// GET /api/v1/events — persistent SSE stream of daemon events.
/// Each message has `event: <name>` and `data: <json-payload>`.
async fn sse_handler(
    State(state): State<ApiState>,
) -> impl IntoResponse {
    let rx = state.daemon.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| async move {
        match result {
            Ok(ev) => {
                let data = ev.payload.to_string();
                Some(Ok::<Event, Infallible>(Event::default().event(ev.event).data(data)))
            }
            // Lagged — skip and continue
            Err(_) => None,
        }
    });

    let sse = Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    );

    // Disable Cloudflare / nginx response buffering so SSE tokens
    // reach the mobile client in real-time instead of being held
    // until the stream ends.
    (
        [
            ("X-Accel-Buffering", "no"),
            ("Cache-Control", "no-cache"),
        ],
        sse,
    )
}
