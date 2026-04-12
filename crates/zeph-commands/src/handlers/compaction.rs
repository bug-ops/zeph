// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context compaction and conversation reset handlers: `/compact`, `/new`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Context compaction handler stub.
///
/// This handler is NOT registered in any command registry. The `/compact` command is
/// dispatched directly via `dispatch_slash_command` because `compact_context` holds `&self`
/// across `.await` points (via `load_compression_guidelines_if_enabled`), making the future
/// non-Send.
///
/// This struct exists to document the intended handler shape for when the Send constraint
/// is lifted in a future migration phase.
pub struct CompactCommand;

impl CommandHandler<CommandContext<'_>> for CompactCommand {
    fn name(&self) -> &'static str {
        "/compact"
    }

    fn description(&self) -> &'static str {
        "Compact the context window"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Session
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.compact_context().await?;
            Ok(CommandOutput::Message(result))
        })
    }
}

/// New conversation handler stub.
///
/// This handler is NOT registered in any command registry. The `/new` command is dispatched
/// directly via `dispatch_slash_command` for the same non-Send reason as `CompactCommand`
/// (calls `load_compression_guidelines_if_enabled` across an `.await`).
///
/// This struct exists to document the intended handler shape for when the Send constraint
/// is lifted in a future migration phase.
pub struct NewConversationCommand;

impl CommandHandler<CommandContext<'_>> for NewConversationCommand {
    fn name(&self) -> &'static str {
        "/new"
    }

    fn description(&self) -> &'static str {
        "Start a new conversation (reset context, preserve memory and MCP)"
    }

    fn args_hint(&self) -> &'static str {
        "[--no-digest] [--keep-plan]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Session
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let keep_plan = args.split_whitespace().any(|a| a == "--keep-plan");
            let no_digest = args.split_whitespace().any(|a| a == "--no-digest");
            let result = ctx.agent.reset_conversation(keep_plan, no_digest).await?;
            Ok(CommandOutput::Message(result))
        })
    }
}
