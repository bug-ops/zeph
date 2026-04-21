// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `logout` (ACP notification, no response).
//!
//! Enabled by feature `unstable-logout`.
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_logout(
//!     notif: acp::schema::LogoutRequest,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Terminates all active sessions for the connected client and clears
//! authentication state, triggering a clean connection close.
