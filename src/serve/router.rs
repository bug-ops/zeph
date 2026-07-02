// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use axum::Router;
use axum::middleware;
use axum::routing::{get, post};
use zeph_common::http_middleware::{AuthConfig, auth_middleware};

use super::AppState;
use super::handlers::{
    create_session_handler, delete_session_handler, events_session_handler, fork_session_handler,
    get_session_handler, health_handler, list_sessions_handler, prompt_session_handler,
};

/// Build the `zeph serve-sessions` axum [`Router`] (spec §9.4).
///
/// Routes:
/// - `GET /health` — unauthenticated liveness check ([`health_handler`])
/// - `POST /sessions` — create a session ([`create_session_handler`])
/// - `GET /sessions` — list live session ids ([`list_sessions_handler`])
/// - `GET /sessions/:id` — durable metadata + live status ([`get_session_handler`])
/// - `DELETE /sessions/:id` — end a live session ([`delete_session_handler`])
/// - `POST /sessions/:id/prompt` — submit a prompt ([`prompt_session_handler`])
/// - `GET /sessions/:id/events` — SSE stream of [`zeph_core::serve::SessionOutput`]
///   ([`events_session_handler`])
/// - `POST /sessions/:id/fork` — eager-copy fork into a fresh live session
///   ([`fork_session_handler`])
///
/// This completes spec §9.4's `/sessions*` surface. Every `/sessions*` route (not `/health`) is
/// layered with [`auth_middleware`] the same way `zeph-gateway`'s router does
/// (`crates/zeph-gateway/src/router.rs`) — constant-time bearer-token check via [`AuthConfig`],
/// keyed off `[serve] require_auth` / `auth_token_vault_key`. No rate limiting is applied here
/// (unlike the gateway): `[serve] max_sessions` and `max_queued_prompts` already bound resource
/// usage per session, which is the more meaningful limit for this API.
pub(super) fn build_router(
    state: AppState,
    auth_token: Option<&str>,
    require_auth: bool,
) -> Router {
    let auth_cfg = AuthConfig::new(auth_token, require_auth);

    let protected = Router::new()
        .route(
            "/sessions",
            post(create_session_handler).get(list_sessions_handler),
        )
        .route(
            "/sessions/{id}",
            get(get_session_handler).delete(delete_session_handler),
        )
        .route("/sessions/{id}/prompt", post(prompt_session_handler))
        .route("/sessions/{id}/events", get(events_session_handler))
        .route("/sessions/{id}/fork", post(fork_session_handler))
        .layer(middleware::from_fn_with_state(auth_cfg, auth_middleware));

    Router::new()
        .route("/health", get(health_handler))
        .merge(protected)
        .with_state(state)
}
