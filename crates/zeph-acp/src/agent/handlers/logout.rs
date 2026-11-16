// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `logout`.

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `logout` request.
pub(crate) async fn handle_logout(
    req: acp::v1::schema::LogoutRequest,
    responder: acp::v1::Responder<acp::v1::schema::LogoutResponse>,
    _cx: acp::v1::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_logout(req).await?;
    responder.respond(resp)
}
