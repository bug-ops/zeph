// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/delete`.

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session/delete` request.
pub(crate) async fn handle_delete_session(
    req: acp::schema::v1::DeleteSessionRequest,
    responder: acp::Responder<acp::schema::v1::DeleteSessionResponse>,
    _cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_delete_session(req).await?;
    responder.respond(resp)
}
