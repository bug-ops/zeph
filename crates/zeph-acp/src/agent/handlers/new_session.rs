// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session.new` (ACP method `"session.new"`).

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session.new` request.
///
/// Creates a new agent session, spawns the agent loop, and returns the session ID.
pub(crate) async fn handle_new_session(
    req: acp::schema::NewSessionRequest,
    responder: acp::Responder<acp::schema::NewSessionResponse>,
    cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_new_session(req, &cx).await?;
    responder.respond(resp)
}
