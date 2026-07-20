// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`SubagentAccess`] — command handler access to sub-agent dispatch and the autonomous
//! goal fleet / sub-agent definition views.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to `/agent` (and `@mention`) dispatch and the `/agents` fleet/definitions view.
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait SubagentAccess {
    // ----- /agent, @mention -----

    /// Dispatch a `/agent` or `@mention` command and return an optional response string.
    ///
    /// `input` is the full trimmed command string. Returns `Ok(None)` when no agent
    /// matched an `@mention` (caller should fall through to LLM processing).
    ///
    /// # Errors
    ///
    /// Returns `Err` when a channel send or subagent operation fails.
    fn handle_agent_dispatch<'a>(
        &'a mut self,
        input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>>;

    // ----- /agents -----

    /// Handle `/agents [subcommand] [args]` and return a formatted response string.
    ///
    /// When called with no arguments or with `fleet`, returns the autonomous goal fleet
    /// view followed by the sub-agent definition list. When called with a CRUD subcommand
    /// (`list`, `show`, `create`, `edit`, `delete`), delegates to the sub-agent manager.
    ///
    /// The default implementation returns an empty string (no output).
    fn handle_agents<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let _ = args;
        Box::pin(async move { Ok(String::new()) })
    }
}
