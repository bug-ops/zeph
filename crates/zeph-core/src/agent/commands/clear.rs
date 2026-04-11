// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handlers for `/clear` and `/reset` slash commands.

use std::future::Future;
use std::pin::Pin;

use crate::channel::Channel;

use super::super::command_registry::{
    CommandContext, CommandHandler, CommandOutput, SlashCategory,
};
use super::super::error::AgentError;

/// Clear conversation history and tool caches without sending a confirmation message.
///
/// Mirrors the logic of `Agent::clear_history()` + the original `/clear` inline cleanup:
/// retains only the first (system prompt) message, clears completed tool IDs, recomputes
/// prompt token count, clears the tool cache, pending images, and URL tracking.
pub(crate) struct ClearCommand;

impl<C: Channel> CommandHandler<C> for ClearCommand {
    fn name(&self) -> &'static str {
        "/clear"
    }

    fn description(&self) -> &'static str {
        "Clear conversation history"
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
            clear_history(ctx);
            Ok(CommandOutput::Silent)
        })
    }
}

/// Reset conversation history (alias for `/clear`, replies with confirmation).
pub(crate) struct ResetCommand;

impl<C: Channel> CommandHandler<C> for ResetCommand {
    fn name(&self) -> &'static str {
        "/reset"
    }

    fn description(&self) -> &'static str {
        "Reset conversation history (alias for /clear, replies with confirmation)"
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
            clear_history(ctx);
            Ok(CommandOutput::Message(
                "Conversation history reset.".to_owned(),
            ))
        })
    }
}

/// Shared history-clearing logic used by both `/clear` and `/reset`.
///
/// Mirrors `Agent::clear_history()` combined with the inline cleanup in the original
/// `handle_builtin_command` handler.
fn clear_history<C: Channel>(ctx: &mut CommandContext<'_, C>) {
    // Keep only the first message (system prompt), matching Agent::clear_history().
    let system_prompt = ctx.msg.messages.first().cloned();
    ctx.msg.messages.clear();
    if let Some(sp) = system_prompt {
        ctx.msg.messages.push(sp);
    }
    // Clear tool dependency state (reset between conversations).
    ctx.tool_state.completed_tool_ids.clear();
    // Recompute cached prompt token count after truncation.
    ctx.providers.cached_prompt_tokens = ctx
        .msg
        .messages
        .iter()
        .map(|m| ctx.metrics.token_counter.count_message_tokens(m) as u64)
        .sum();
    // Clear runtime per-turn caches.
    ctx.msg.pending_image_parts.clear();
    ctx.tool_orchestrator.clear_cache();
    ctx.security.user_provided_urls.write().clear();
}
