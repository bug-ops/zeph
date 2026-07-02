// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/conv` slash command handler — browse durable conversation-sessions (spec-068, #5343).
//!
//! Channel-agnostic by construction (the `CommandHandler`/`ChannelSink` pattern): works
//! identically whether typed in the CLI, TUI, or Telegram, mirroring `zeph serve-sessions`'s
//! `GET /sessions`/`GET /sessions/:id` REST endpoints but reading through
//! [`crate::AgentAccess::handle_conv`] instead of HTTP.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// List, inspect, resume, or fork durable conversation-sessions
/// (`acp_sessions` / `[session] data_dir`).
///
/// Syntax: `/conv` or `/conv list` — list sessions; `/conv show <id>` — one session's metadata;
/// `/conv resume <id>` — live-swap this conversation onto an existing session, replaying its
/// durable log; `/conv fork <id>` — eager-copy `id` into a fresh session and swap onto it
/// (spec-068 §9, D-9).
pub struct ConvCommand;

impl CommandHandler<CommandContext<'_>> for ConvCommand {
    fn name(&self) -> &'static str {
        "/conv"
    }

    fn description(&self) -> &'static str {
        "List, inspect, resume, or fork durable conversation-sessions"
    }

    fn args_hint(&self) -> &'static str {
        "[list | show <id> | resume <id> | fork <id>]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Session
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("session")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.conv.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_conv(args).await?;
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
    fn conv_name_and_description() {
        assert_eq!(ConvCommand.name(), "/conv");
        assert!(!ConvCommand.description().is_empty());
    }

    #[tokio::test]
    async fn conv_not_supported_returns_ok_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let result = ConvCommand.handle(&mut ctx, "").await;
        assert!(result.is_ok());
        if let Ok(CommandOutput::Message(msg)) = result {
            assert!(!msg.is_empty());
        }
    }
}
