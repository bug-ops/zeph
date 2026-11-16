// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `authenticate` (ACP method `"authenticate"`).

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `authenticate` request.
pub(crate) async fn handle_authenticate(
    req: acp::v1::schema::AuthenticateRequest,
    responder: acp::v1::Responder<acp::v1::schema::AuthenticateResponse>,
    _cx: acp::v1::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_authenticate(req).await?;
    responder.respond(resp)
}
