// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/set_mode` (ACP method `"session/set_mode"`).
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_set_session_mode(
//!     req: acp::schema::SetSessionModeRequest,
//!     responder: acp::Responder<acp::schema::SetSessionModeResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Updates the active session's operating mode (e.g., `auto`, `manual`)
//! and persists the change in `state.sessions`.
