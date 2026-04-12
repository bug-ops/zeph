// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MCP management handler: `/mcp`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// MCP management handler stub.
///
/// This handler is NOT registered in any command registry. The `/mcp` command is dispatched
/// directly via `dispatch_slash_command` because `handle_mcp_command` holds a
/// `RwLockGuard<McpRegistry>` across `.await` points, making the future non-Send.
///
/// This struct exists to document the intended handler shape for when the Send constraint
/// is lifted in a future migration phase.
pub struct McpCommand;

impl CommandHandler<CommandContext<'_>> for McpCommand {
    fn name(&self) -> &'static str {
        "/mcp"
    }

    fn description(&self) -> &'static str {
        "Manage MCP servers"
    }

    fn args_hint(&self) -> &'static str {
        "[add|list|tools|remove]"
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
            let result = ctx.agent.handle_mcp(args).await?;
            if result.is_empty() {
                Ok(CommandOutput::Silent)
            } else {
                Ok(CommandOutput::Message(result))
            }
        })
    }
}
