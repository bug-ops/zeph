// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MCP management handler: `/mcp`.
//!
//! Delegates to `AgentAccess::handle_mcp`, which in turn calls the
//! `Agent<C>` inherent methods in `zeph-core::agent::mcp`. Status messages
//! (`send_status`) are emitted as channel side effects inside the `Agent<C>`
//! implementation; only the final user-facing message is surfaced as the
//! command return value.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Manage MCP server connections.
///
/// Subcommands: `add`, `list`, `tools`, `remove`.
///
/// Delegates to `AgentAccess::handle_mcp`; the actual MCP work and
/// real-time status indicators run inside `Agent<C>` which has direct channel
/// access.
pub struct McpCommand;

impl CommandHandler<CommandContext<'_>> for McpCommand {
    fn name(&self) -> &'static str {
        "/mcp"
    }

    fn description(&self) -> &'static str {
        "Manage MCP server connections"
    }

    fn args_hint(&self) -> &'static str {
        "add|list|tools|remove"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Integration
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            // All MCP output is sent directly via the channel inside handle_mcp.
            // The returned string is empty — we use Silent to avoid double-sending.
            let _ = ctx.agent.handle_mcp(args).await?;
            Ok(CommandOutput::Silent)
        })
    }
}
