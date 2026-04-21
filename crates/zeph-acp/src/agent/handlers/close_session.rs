// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/close` (ACP method `"session/close"`).
//!
//! Enabled by feature `unstable-session-close`.
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_close_session(
//!     req: acp::schema::CloseSessionRequest,
//!     responder: acp::Responder<acp::schema::CloseSessionResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Terminates the specified session, persists final state if requested,
//! and frees all associated resources.
