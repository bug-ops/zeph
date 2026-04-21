// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/resume` (ACP method `"session/resume"`).
//!
//! Enabled by feature `unstable-session-resume`.
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_resume_session(
//!     req: acp::schema::ResumeSessionRequest,
//!     responder: acp::Responder<acp::schema::ResumeSessionResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Re-attaches the client to a previously detached session, restoring
//! the streaming output channel and returning current session state.
