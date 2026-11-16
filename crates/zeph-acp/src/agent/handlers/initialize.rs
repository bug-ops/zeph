// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `initialize` (ACP method `"initialize"`).

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `initialize` request.
///
/// Stores client capabilities and responds with the agent's capabilities.
pub(crate) async fn handle_initialize(
    req: acp::v1::schema::InitializeRequest,
    responder: acp::v1::Responder<acp::v1::schema::InitializeResponse>,
    _cx: acp::v1::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_initialize(req).await?;
    responder.respond(resp)
}
