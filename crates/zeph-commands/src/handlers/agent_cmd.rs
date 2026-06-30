// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sub-agent management handler: `/agent`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Manage sub-agents or dispatch `@mention` commands.
///
/// Delegates to `AgentAccess::handle_agent_dispatch`, which handles both `/agent`
/// subcommands and `@name` mentions. Returns `Continue` when the dispatch returns
/// `None` (no agent matched an `@mention` — fall through to LLM processing).
pub struct AgentCommand;

impl CommandHandler<CommandContext<'_>> for AgentCommand {
    fn name(&self) -> &'static str {
        "/agent"
    }

    fn description(&self) -> &'static str {
        "Manage sub-agents"
    }

    fn args_hint(&self) -> &'static str {
        "[subcommand]"
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
        let span = tracing::info_span!("commands.agent.handle");
        Box::pin(
            async move {
                let input = if args.is_empty() {
                    "/agent".to_owned()
                } else {
                    format!("/agent {args}")
                };
                match ctx.agent.handle_agent_dispatch(&input).await? {
                    Some(msg) => Ok(CommandOutput::Message(msg)),
                    None => Ok(CommandOutput::Silent),
                }
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
    fn agent_cmd_name_and_description() {
        assert_eq!(AgentCommand.name(), "/agent");
        assert!(!AgentCommand.description().is_empty());
    }

    #[tokio::test]
    async fn agent_dispatch_none_returns_silent() {
        // NullAgent returns Ok(None), so the handler returns Silent.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = AgentCommand.handle(&mut ctx, "").await.unwrap();
        assert_matches!(out, CommandOutput::Silent);
    }

    #[tokio::test]
    async fn agent_dispatch_with_args_returns_silent() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = AgentCommand.handle(&mut ctx, "list").await.unwrap();
        assert_matches!(out, CommandOutput::Silent);
    }
}
