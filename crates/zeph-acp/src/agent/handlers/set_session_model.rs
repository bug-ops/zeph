// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/set_model` (feature `unstable-session-model`).

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session/set_model` request.
pub(crate) async fn handle_set_session_model(
    req: acp::schema::SetSessionModelRequest,
    responder: acp::Responder<acp::schema::SetSessionModelResponse>,
    _cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_set_session_model(req).await?;
    responder.respond(resp)
}
