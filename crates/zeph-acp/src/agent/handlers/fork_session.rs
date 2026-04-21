// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/fork` (ACP method `"session/fork"`).
//!
//! Enabled by feature `unstable-session-fork`.
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_fork_session(
//!     req: acp::schema::ForkSessionRequest,
//!     responder: acp::Responder<acp::schema::ForkSessionResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Creates a copy of an existing session at a specified history checkpoint,
//! returning the new session's ID and metadata.
