// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LSP status command handler: `/lsp`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Show LSP context injection status and session statistics.
pub struct LspCommand;

impl CommandHandler<CommandContext<'_>> for LspCommand {
    fn name(&self) -> &'static str {
        "/lsp"
    }

    fn description(&self) -> &'static str {
        "Show LSP context status"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("lsp-context")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.lsp_status().await?;
            if result.is_empty() {
                Ok(CommandOutput::Silent)
            } else {
                Ok(CommandOutput::Message(result))
            }
        })
    }
}
