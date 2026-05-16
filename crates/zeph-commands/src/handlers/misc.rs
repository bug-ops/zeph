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
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.cache_stats.handle");
        Box::pin(
            async move {
                let result = ctx.agent.cache_stats();
                Ok(CommandOutput::Message(result))
            }
            .instrument(span),
        )
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
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.notify_test.handle");
        Box::pin(
            async move {
                let result = ctx.agent.notify_test().await?;
                Ok(CommandOutput::Message(result))
            }
            .instrument(span),
        )
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
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.image.handle");
        Box::pin(
            async move {
                if args.is_empty() {
                    return Ok(CommandOutput::Message("Usage: /image <path>".to_owned()));
                }
                let result = ctx.agent.load_image(args).await?;
                Ok(CommandOutput::Message(result))
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

    #[test]
    fn cache_stats_name_and_description() {
        assert_eq!(CacheStatsCommand.name(), "/cache-stats");
        assert!(!CacheStatsCommand.description().is_empty());
    }

    #[test]
    fn notify_test_name_and_description() {
        assert_eq!(NotifyTestCommand.name(), "/notify-test");
        assert!(!NotifyTestCommand.description().is_empty());
    }

    #[test]
    fn image_name_and_description() {
        assert_eq!(ImageCommand.name(), "/image");
        assert!(!ImageCommand.description().is_empty());
    }

    #[tokio::test]
    async fn cache_stats_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = CacheStatsCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }

    #[tokio::test]
    async fn notify_test_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = NotifyTestCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }

    #[tokio::test]
    async fn image_no_args_returns_usage_hint() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ImageCommand.handle(&mut ctx, "").await.unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("/image"));
    }

    #[tokio::test]
    async fn image_with_path_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ImageCommand
            .handle(&mut ctx, "/tmp/photo.png")
            .await
            .unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }
}
