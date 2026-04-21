// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/resume` (feature `unstable-session-resume`).

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session/resume` request.
pub(crate) async fn handle_resume_session(
    req: acp::schema::ResumeSessionRequest,
    responder: acp::Responder<acp::schema::ResumeSessionResponse>,
    cx: acp::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_resume_session(req, &cx).await?;
    responder.respond(resp)
}
