// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for extension method dispatch (ACP `on_receive_dispatch`).
//!
//! Receives all unrecognised methods as `acp::Dispatch` and routes them
//! to the custom extension handler in `custom.rs`.
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_dispatch(
//!     message: acp::Dispatch,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Forwards the raw `acp::Dispatch` message to the extension handler;
//! responds with an internal error for unknown methods.
