// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Miscellaneous utility handlers: `/cache-stats`, `/image`, `/notify-test`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Display tool orchestrator cache statistics.
pub struct CacheStatsCommand;

impl CommandHandler<CommandContext<'_>> for CacheStatsCommand {
    fn name(&self) -> &'static str {
        "/cache-stats"
    }

    fn description(&self) -> &'static str {
        "Show tool orchestrator cache statistics"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.cache_stats();
            Ok(CommandOutput::Message(result))
        })
    }
}

/// Send a test notification via all enabled notification channels.
pub struct NotifyTestCommand;

impl CommandHandler<CommandContext<'_>> for NotifyTestCommand {
    fn name(&self) -> &'static str {
        "/notify-test"
    }

    fn description(&self) -> &'static str {
        "Send a test notification via all enabled channels (macOS, webhook)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.notify_test().await?;
            Ok(CommandOutput::Message(result))
        })
    }
}

/// Attach an image file to the next user message.
///
/// `args` must be a non-empty file path. If `args` is empty the handler returns
/// a usage hint.
pub struct ImageCommand;

impl CommandHandler<CommandContext<'_>> for ImageCommand {
    fn name(&self) -> &'static str {
        "/image"
    }

    fn description(&self) -> &'static str {
        "Attach an image to the next message"
    }

    fn args_hint(&self) -> &'static str {
        "<path>"
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
            if args.is_empty() {
                return Ok(CommandOutput::Message("Usage: /image <path>".to_owned()));
            }
            let result = ctx.agent.load_image(args).await?;
            Ok(CommandOutput::Message(result))
        })
    }
}
