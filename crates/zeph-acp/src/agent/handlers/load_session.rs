// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/load` (ACP method `"session/load"`).
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_load_session(
//!     req: acp::schema::LoadSessionRequest,
//!     responder: acp::Responder<acp::schema::LoadSessionResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Loads a previously persisted session from storage and restores its
//! agent loop state, responding with the session metadata.
