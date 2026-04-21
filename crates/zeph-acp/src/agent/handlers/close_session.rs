// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/close` (feature `unstable-session-close`).

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session/close` request.
pub(crate) async fn handle_close_session(
    req: acp::schema::CloseSessionRequest,
    responder: acp::Responder<acp::schema::CloseSessionResponse>,
    _cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_close_session(req).await?;
    responder.respond(resp)
}
