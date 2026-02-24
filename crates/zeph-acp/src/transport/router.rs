#[cfg(feature = "acp-http")]
use axum::Router;
#[cfg(feature = "acp-http")]
use axum::extract::DefaultBodyLimit;
#[cfg(feature = "acp-http")]
use axum::routing::{get, post};
#[cfg(feature = "acp-http")]
use tower_http::cors::CorsLayer;

#[cfg(feature = "acp-http")]
use crate::transport::http::{AcpHttpState, get_handler, post_handler};
#[cfg(feature = "acp-http")]
use crate::transport::ws::ws_upgrade_handler;

/// HTTP body size limit: 1 MiB — large enough for any JSON-RPC request.
#[cfg(feature = "acp-http")]
const MAX_BODY_BYTES: usize = 1_048_576;

/// Build the axum `Router` for the ACP HTTP+SSE and WebSocket endpoints.
///
/// Routes:
/// - `POST /acp` — JSON-RPC request body (≤ 1 MiB), SSE response stream
/// - `GET /acp` — SSE notification reconnect (requires `Acp-Session-Id`)
/// - `GET /acp/ws` — WebSocket upgrade
///
/// Security layers applied:
/// - `DefaultBodyLimit::max(1_048_576)` — rejects oversized POST bodies
/// - `CorsLayer` with empty origin list — denies all cross-origin requests
#[cfg(feature = "acp-http")]
pub fn acp_router(state: AcpHttpState) -> Router {
    Router::new()
        .route("/acp", post(post_handler).get(get_handler))
        .route("/acp/ws", get(ws_upgrade_handler))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(CorsLayer::new())
        .with_state(state)
}
