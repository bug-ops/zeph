// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `session/cancel` (ACP notification, no response).
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_cancel(
//!     notif: acp::schema::CancelNotification,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Sends an abort signal to the active session task and waits for clean shutdown.
