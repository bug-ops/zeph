// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `/exit` and `/quit` slash commands.

use std::future::Future;
use std::pin::Pin;

use crate::channel::Channel;

use super::super::command_registry::{
    CommandContext, CommandHandler, CommandOutput, SlashCategory,
};
use super::super::error::AgentError;

/// Exit the agent loop.
///
/// `/exit` and `/quit` are treated as aliases; both map to this handler via the registry.
/// When the channel does not support exit (e.g., Telegram), the command is rejected with
/// a user-visible message.
pub(crate) struct ExitCommand;

impl<C: Channel> CommandHandler<C> for ExitCommand {
    fn name(&self) -> &'static str {
        "/exit"
    }

    fn description(&self) -> &'static str {
        "Exit the agent (also: /quit)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Session
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_, C>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, AgentError>> + Send + 'a>> {
        Box::pin(async move {
            if ctx.channel.supports_exit() {
                Ok(CommandOutput::Exit)
            } else {
                ctx.channel
                    .send("/exit is not supported in this channel.")
                    .await?;
                Ok(CommandOutput::Continue)
            }
        })
    }
}

/// Alias for `/exit`.
pub(crate) struct QuitCommand;

impl<C: Channel> CommandHandler<C> for QuitCommand {
    fn name(&self) -> &'static str {
        "/quit"
    }

    fn description(&self) -> &'static str {
        "Exit the agent (alias for /exit)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Session
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_, C>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, AgentError>> + Send + 'a>> {
        Box::pin(async move {
            if ctx.channel.supports_exit() {
                Ok(CommandOutput::Exit)
            } else {
                ctx.channel
                    .send("/exit is not supported in this channel.")
                    .await?;
                Ok(CommandOutput::Continue)
            }
        })
    }
}
