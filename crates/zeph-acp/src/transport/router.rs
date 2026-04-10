// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Axum router construction for the ACP HTTP transport.
//!
//! The router is the single axum `Router` that wires up all ACP endpoints.
//! Callers attach it to their axum `Server` and call
//! [`AcpHttpState::mark_ready`](crate::transport::http::AcpHttpState::mark_ready) after
//! initialization.

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
use crate::transport::discovery::{agent_json_handler, discovery_handler};
#[cfg(feature = "acp-http")]
use crate::transport::http::{
    AcpHttpState, get_handler, health_handler, list_sessions_handler, post_handler,
    session_messages_handler,
};
#[cfg(feature = "acp-http")]
use crate::transport::ws::ws_upgrade_handler;

/// HTTP body size limit: 1 MiB — large enough for any JSON-RPC request.
#[cfg(feature = "acp-http")]
const MAX_BODY_BYTES: usize = 1_048_576;

/// Build the axum [`Router`] for the ACP HTTP+SSE and WebSocket endpoints.
///
/// Attach the returned router to an axum `Server`, then call
/// [`AcpHttpState::mark_ready`](crate::transport::http::AcpHttpState::mark_ready)
/// after all initialization is complete.
///
/// # Routes
///
/// | Method | Path | Description |
/// |--------|------|-------------|
/// | `POST` | `/acp` | JSON-RPC request body (≤ 1 MiB), SSE response stream |
/// | `GET` | `/acp` | SSE notification reconnect (requires `Acp-Session-Id` header) |
/// | `GET` | `/acp/ws` | WebSocket upgrade |
/// | `GET` | `/health` | Public readiness probe |
/// | `GET` | `/.well-known/acp.json` | Discovery manifest (always public, no auth) |
/// | `GET` | `/agent.json` | Agent identity manifest for ACP Registry (always public) |
///
/// # Security layers
///
/// - `DefaultBodyLimit::max(1_048_576)` — rejects oversized POST bodies
/// - `CorsLayer` with empty origin list — denies all cross-origin requests by default
/// - `BearerAuthLayer` — applied to `/acp` routes when `auth_bearer_token` is `Some`
///
/// # Examples
///
/// ```rust,no_run
/// # #[cfg(feature = "acp-http")]
/// # {
/// use std::sync::Arc;
/// use zeph_acp::{AgentSpawner, AcpServerConfig, AcpHttpState, acp_router};
///
/// let spawner: AgentSpawner = Arc::new(|ch, ctx, sess| Box::pin(async move { drop((ch, ctx, sess)); }));
/// let config = AcpServerConfig { agent_name: "zeph".to_owned(), ..AcpServerConfig::default() };
/// let state = AcpHttpState::new(spawner, config);
/// state.mark_ready();
///
/// let router = acp_router(state);
/// // attach `router` to axum::serve(...)
/// # }
/// ```
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

    let mut router = Router::new()
        .route("/health", get(health_handler))
        .merge(acp_routes);

    if state.server_config.discovery_enabled {
        router = router
            .route("/.well-known/acp.json", get(discovery_handler))
            .route("/agent.json", get(agent_json_handler));
    }

    router.with_state(state)
}
