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
/// Delegates to `AgentAccess::handle_mcp`, which collects all output into
/// a `String` and returns it.  The registry sends the string to the channel
/// as a `Message` output.
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
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.mcp.handle");
        Box::pin(
            async move {
                let output = ctx.agent.handle_mcp(args).await?;
                Ok(CommandOutput::Message(output))
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::test_helpers::{MockDebug, MockMessages, MockSession, make_ctx};
    use crate::sink::NullSink;
    use std::assert_matches;

    #[test]
    fn mcp_name_and_description() {
        assert_eq!(McpCommand.name(), "/mcp");
        assert!(!McpCommand.description().is_empty());
    }

    #[tokio::test]
    async fn mcp_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = McpCommand.handle(&mut ctx, "list").await.unwrap();
        assert_matches!(out, CommandOutput::Message(_));
    }
}
