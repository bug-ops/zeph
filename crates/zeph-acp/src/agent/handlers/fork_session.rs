// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/fork` (feature `unstable-session-fork`).

use std::sync::Arc;

use agent_client_protocol as acp;

use crate::agent::ZephAcpAgentState;

/// Handle an ACP `session/fork` request.
#[cfg_attr(
    feature = "gonka",
    expect(
        clippy::large_futures,
        reason = "Gonka provider state increases the fork handler future size beyond the lint threshold."
    )
)]
pub(crate) async fn handle_fork_session(
    req: acp::v1::schema::ForkSessionRequest,
    responder: acp::v1::Responder<acp::v1::schema::ForkSessionResponse>,
    cx: acp::v1::ConnectionTo<acp::Client>,
    state: Arc<ZephAcpAgentState>,
) -> acp::Result<()> {
    let resp = state.do_fork_session(req, &cx).await?;
    responder.respond(resp)
}
