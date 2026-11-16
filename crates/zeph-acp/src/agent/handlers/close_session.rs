// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/close`.

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session/close` request.
pub(crate) async fn handle_close_session(
    req: acp::v1::schema::CloseSessionRequest,
    responder: acp::v1::Responder<acp::v1::schema::CloseSessionResponse>,
    _cx: acp::v1::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_close_session(req).await?;
    responder.respond(resp)
}
