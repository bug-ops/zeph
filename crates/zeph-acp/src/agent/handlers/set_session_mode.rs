// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/set_mode`.

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session/set_mode` request.
pub(crate) async fn handle_set_session_mode(
    req: acp::v1::schema::SetSessionModeRequest,
    responder: acp::v1::Responder<acp::v1::schema::SetSessionModeResponse>,
    _cx: acp::v1::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_set_session_mode(req).await?;
    responder.respond(resp)
}
