// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MCP management handler: `/mcp`.
//!
//! Note: a `McpCommand` struct is intentionally absent. `AgentAccess::handle_mcp` cannot be
//! made `Send` due to HRTB constraints on `tokio::sync::RwLockWriteGuard` inside
//! `McpManager::add_server` / `remove_server` — the guard is held across `.await`, which
//! fails the `for<'a> &'a Agent<C>: Send` bound in a `Box<dyn Future + Send>`. `/mcp`
//! remains in `dispatch_slash_command` until `McpManager` is refactored to avoid holding the
//! write guard across await boundaries.
