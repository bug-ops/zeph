// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/list` (ACP method `"session/list"`).
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_list_sessions(
//!     req: acp::schema::ListSessionsRequest,
//!     responder: acp::Responder<acp::schema::ListSessionsResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Returns the list of all active and persisted sessions for the connected client.
