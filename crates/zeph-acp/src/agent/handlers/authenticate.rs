// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `authenticate` (ACP method `"authenticate"`).
//!
//! # PR 2 contract
//!
//! ```ignore
//! pub(crate) async fn handle_authenticate(
//!     req: acp::schema::AuthenticateRequest,
//!     responder: acp::Responder<acp::schema::AuthenticateResponse>,
//!     cx: acp::ConnectionTo<acp::Client>,
//!     state: Arc<ZephAcpAgentState>,
//! ) -> acp::Result<()>
//! ```
//!
//! Validates credentials supplied by the client and responds with an
//! authentication token or an error if authentication fails.
