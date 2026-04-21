// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/new` (ACP method `"session/new"`).
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_new_session(
//!     req: acp::schema::NewSessionRequest,
//!     responder: acp::Responder<acp::schema::NewSessionResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Creates a new agent session, spawns the agent loop on a `LoopbackChannel`,
//! and responds with the session ID and initial session metadata.
