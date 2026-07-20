// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`McpAccess`] — command handler access to the MCP client subsystem.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to the `/mcp` command (add/list/tools/remove MCP servers).
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait McpAccess {
    // ----- /mcp -----

    /// Handle `/mcp [add|list|tools|remove]` and send output via the agent channel.
    ///
    /// Returns `Ok(())` on success. Intermediate messages are sent directly by the
    /// `Agent<C>` implementation via `self.channel`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when a channel send or MCP operation fails.
    fn handle_mcp<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;
}
