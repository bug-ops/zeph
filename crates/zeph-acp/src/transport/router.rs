// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "acp-http")]
use axum::Router;
#[cfg(feature = "acp-http")]
use axum::extract::DefaultBodyLimit;
#[cfg(feature = "acp-http")]
use axum::routing::{get, post};
#[cfg(feature = "acp-http")]
use tower_http::cors::CorsLayer;

#[cfg(feature = "acp-http")]
use crate::transport::auth::BearerAuthLayer;
#[cfg(feature = "acp-http")]
use crate::transport::discovery::discovery_handler;
#[cfg(feature = "acp-http")]
use crate::transport::http::{
    AcpHttpState, get_handler, list_sessions_handler, post_handler, session_messages_handler,
};
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
/// - `GET /.well-known/acp.json` — discovery manifest (always public, no auth)
///
/// Security layers applied:
/// - `DefaultBodyLimit::max(1_048_576)` — rejects oversized POST bodies
/// - `CorsLayer` with empty origin list — denies all cross-origin requests
/// - `BearerAuthLayer` — applied to /acp routes when `auth_bearer_token` is `Some`
#[cfg(feature = "acp-http")]
pub fn acp_router(state: AcpHttpState) -> Router {
    let acp_routes = Router::new()
        .route("/acp", post(post_handler).get(get_handler))
        .route("/acp/ws", get(ws_upgrade_handler))
        .route("/sessions", get(list_sessions_handler))
        .route("/sessions/{id}/messages", get(session_messages_handler))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(CorsLayer::new());

    let acp_routes = if let Some(token) = state.server_config.auth_bearer_token.clone() {
        acp_routes.layer(BearerAuthLayer::new(token))
    } else {
        tracing::warn!(
            "ACP HTTP server started without bearer token authentication; \
             session history endpoints are publicly accessible"
        );
        acp_routes
    };

    let mut router = Router::new().merge(acp_routes);

    if state.server_config.discovery_enabled {
        router = router.route("/.well-known/acp.json", get(discovery_handler));
    }

    router.with_state(state)
}
