// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Low-level observability handler for the real ACP `$/cancel_request` protocol notification.
//!
//! `$/cancel_request` carries only a JSON-RPC `request_id`, not a `session_id`, so it cannot be
//! mapped to a `SessionEntry::cancel_signal` here. The SDK already updates per-request
//! cancellation markers automatically once `unstable_cancel_request` is enabled — registering a
//! handler does not replace that built-in behavior, it only adds visibility. The functional
//! bridge onto `cancel_signal` lives in `handlers/prompt.rs`, scoped to the `session/prompt`
//! request via `Responder::cancellation()`, which already knows the originating session.

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Observe an incoming `$/cancel_request` notification for tracing purposes.
pub(crate) async fn handle_cancel_request(
    notif: acp::schema::v1::CancelRequestNotification,
    _cx: acp::ConnectionTo<acp::Client>,
    _state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    tracing::debug!(request_id = ?notif.request_id, "received $/cancel_request");
    Ok(())
}
