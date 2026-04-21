// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/prompt` (ACP method `"session/prompt"`).

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `prompt` request.
pub(crate) async fn handle_prompt(
    req: acp::schema::PromptRequest,
    responder: acp::Responder<acp::schema::PromptResponse>,
    _cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_prompt(req).await?;
    responder.respond(resp)
}
