// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/set_config_option`.

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session/set_config_option` request.
pub(crate) async fn handle_set_session_config_option(
    req: acp::schema::SetSessionConfigOptionRequest,
    responder: acp::Responder<acp::schema::SetSessionConfigOptionResponse>,
    _cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_set_session_config_option(req).await?;
    responder.respond(resp)
}
