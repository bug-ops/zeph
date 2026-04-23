// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/acp` slash command handler.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Inspect ACP server configuration (`/acp dirs`, `/acp auth-methods`, `/acp status`).
pub struct AcpCommand;

impl CommandHandler<CommandContext<'_>> for AcpCommand {
    fn name(&self) -> &'static str {
        "/acp"
    }

    fn description(&self) -> &'static str {
        "Inspect ACP server configuration (dirs, auth-methods, status)"
    }

    fn args_hint(&self) -> &'static str {
        "[dirs | auth-methods | status]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Integration
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("acp")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move { Ok(CommandOutput::Message(ctx.agent.handle_acp(args).await?)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_slash_acp() {
        assert_eq!(AcpCommand.name(), "/acp");
    }

    #[test]
    fn category_is_integration() {
        assert_eq!(AcpCommand.category(), SlashCategory::Integration);
    }

    #[test]
    fn feature_gate_is_acp() {
        assert_eq!(AcpCommand.feature_gate(), Some("acp"));
    }

    #[test]
    fn description_and_args_hint_non_empty() {
        assert!(!AcpCommand.description().is_empty());
        assert!(!AcpCommand.args_hint().is_empty());
    }
}
