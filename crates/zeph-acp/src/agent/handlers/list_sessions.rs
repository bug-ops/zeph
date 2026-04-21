// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/list`.

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session/list` request.
pub(crate) async fn handle_list_sessions(
    req: acp::schema::ListSessionsRequest,
    responder: acp::Responder<acp::schema::ListSessionsResponse>,
    _cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_list_sessions(req).await?;
    responder.respond(resp)
}
