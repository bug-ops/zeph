// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `initialize` (ACP method `"initialize"`).
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_initialize(
//!     req: acp::schema::InitializeRequest,
//!     responder: acp::Responder<acp::schema::InitializeResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Validates client capabilities, persists them in `state.client_caps`,
//! and responds with the agent's capabilities and available models.
