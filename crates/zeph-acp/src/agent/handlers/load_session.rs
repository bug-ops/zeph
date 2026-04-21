// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/load`.

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session/load` request.
pub(crate) async fn handle_load_session(
    req: acp::schema::LoadSessionRequest,
    responder: acp::Responder<acp::schema::LoadSessionResponse>,
    cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_load_session(req, &cx).await?;
    responder.respond(resp)
}
