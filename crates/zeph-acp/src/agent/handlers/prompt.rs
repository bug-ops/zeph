// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/prompt` (ACP method `"session/prompt"`).
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_prompt(
//!     req: acp::schema::PromptRequest,
//!     responder: acp::Responder<acp::schema::PromptResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Forwards the user prompt to the active session's agent loop, streams
//! output events back to the client, and responds with the final result.
