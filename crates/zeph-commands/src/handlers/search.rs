// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/search` slash command handler.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Issue a `web_search` tool call for a natural-language query.
///
/// Syntax: `/search <query> [--limit N]`. Requires `[tools.search].enabled = true` and a
/// resolved API key (spec 006-1-web-search) — otherwise returns a message explaining how
/// to enable it, rather than an error.
pub struct SearchCommand;

impl CommandHandler<CommandContext<'_>> for SearchCommand {
    fn name(&self) -> &'static str {
        "/search"
    }

    fn description(&self) -> &'static str {
        "Search the web for a natural-language query (requires tools.search.enabled)"
    }

    fn args_hint(&self) -> &'static str {
        "<query> [--limit N]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Integration
    }

    fn requires_auth(&self) -> bool {
        true
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.search.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_web_search(args).await?;
                Ok(CommandOutput::Message(result))
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandRegistry;
    use crate::handlers::test_helpers::{MockDebug, MockMessages, MockSession, make_ctx};
    use crate::sink::NullSink;

    #[test]
    fn search_name_and_description() {
        assert_eq!(SearchCommand.name(), "/search");
        assert!(!SearchCommand.description().is_empty());
    }

    #[tokio::test]
    async fn search_not_supported_returns_ok_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let result = SearchCommand.handle(&mut ctx, "rust async").await;
        assert!(result.is_ok());
        if let Ok(CommandOutput::Message(msg)) = result {
            assert!(!msg.is_empty());
        }
    }

    #[tokio::test]
    async fn search_dispatch_rejected_when_untrusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(SearchCommand);

        let result = reg.dispatch(&mut ctx, "/search rust async", false).await;
        let err = result.unwrap().unwrap_err();
        assert!(err.0.contains("trusted"));
    }

    #[tokio::test]
    async fn search_dispatch_allowed_when_trusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(SearchCommand);

        let result = reg.dispatch(&mut ctx, "/search rust async", true).await;
        assert!(result.unwrap().is_ok());
    }
}
