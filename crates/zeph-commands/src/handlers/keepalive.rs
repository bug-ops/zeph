// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/keepalive` command handler — session control of the prompt-cache keepalive.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Show or switch the prompt-cache keepalive for this session.
///
/// - `/keepalive` or `/keepalive status` — report the service state.
/// - `/keepalive on` — arm it, clearing any stop, even when `[llm.cache_keepalive]` is off.
/// - `/keepalive off` — stop pings until `on`.
///
/// Session-only: never persisted. Pings cost tokens at the cache-read price, so the service is
/// opt-in and capped by `max_consecutive_pings`.
pub struct KeepaliveCommand;

impl CommandHandler<CommandContext<'_>> for KeepaliveCommand {
    fn name(&self) -> &'static str {
        "/keepalive"
    }

    fn description(&self) -> &'static str {
        "Show or switch the prompt-cache keepalive for this session"
    }

    fn args_hint(&self) -> &'static str {
        "[on|off|status]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Configuration
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
        let span = tracing::info_span!("commands.keepalive.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_keepalive(args).await;
                Ok(CommandOutput::message_or_silent(result))
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
    use std::assert_matches;

    #[test]
    fn keepalive_name_and_description() {
        assert_eq!(KeepaliveCommand.name(), "/keepalive");
        assert_ne!(KeepaliveCommand.description(), "");
    }

    #[tokio::test]
    async fn keepalive_returns_silent_when_agent_returns_empty() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = KeepaliveCommand.handle(&mut ctx, "on").await.unwrap();
        assert_matches!(out, CommandOutput::Silent);
    }
}
