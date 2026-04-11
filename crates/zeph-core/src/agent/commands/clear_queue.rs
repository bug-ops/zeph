// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `/clear-queue` slash command.

use std::future::Future;
use std::pin::Pin;

use crate::channel::Channel;

use super::super::command_registry::{
    CommandContext, CommandHandler, CommandOutput, SlashCategory,
};
use super::super::error::AgentError;

/// Discard all messages currently queued for processing.
pub(crate) struct ClearQueueCommand;

impl<C: Channel> CommandHandler<C> for ClearQueueCommand {
    fn name(&self) -> &'static str {
        "/clear-queue"
    }

    fn description(&self) -> &'static str {
        "Discard queued messages"
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
            let n = ctx.msg.message_queue.len();
            ctx.msg.message_queue.clear();
            // Notify channel of updated queue count (no-op for most channels).
            let _ = ctx.channel.send_queue_count(0).await;
            Ok(CommandOutput::Message(format!(
                "Cleared {n} queued messages."
            )))
        })
    }
}
