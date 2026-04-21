// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/set_config_option` (ACP method `"session/set_config_option"`).
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_set_session_config_option(
//!     req: acp::schema::SetSessionConfigOptionRequest,
//!     responder: acp::Responder<acp::schema::SetSessionConfigOptionResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Applies a single named configuration option to the active session,
//! persisting the change without restarting the agent loop.
