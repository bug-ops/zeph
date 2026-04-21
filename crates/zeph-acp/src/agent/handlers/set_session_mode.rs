// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/set_mode`.

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session/set_mode` request.
pub(crate) async fn handle_set_session_mode(
    req: acp::schema::SetSessionModeRequest,
    responder: acp::Responder<acp::schema::SetSessionModeResponse>,
    _cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_set_session_mode(req).await?;
    responder.respond(resp)
}
