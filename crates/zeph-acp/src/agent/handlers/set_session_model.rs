// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/set_model` (ACP method `"session/set_model"`).
//!
//! Enabled by feature `unstable-session-model`.
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_set_session_model(
//!     req: acp::schema::SetSessionModelRequest,
//!     responder: acp::Responder<acp::schema::SetSessionModelResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Switches the active session to a different LLM model, updating
//! `state.sessions` and applying the change to subsequent prompt turns.
