// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/goal` slash command handler.
//!
//! Subcommands:
//! - `create <text> [--budget N]` — create a new goal, pausing any existing active one
//! - `pause` — pause the active goal
//! - `resume` — resume the last paused goal
//! - `complete` — mark the active goal as completed
//! - `clear` — dismiss the active or paused goal
//! - `status` — show the active goal and recent history
//! - `list` — list all goals (active, paused, completed, cleared)

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Manage long-horizon goals that span multiple conversation turns.
///
/// At most one goal can be `active` at a time. Creating a new goal auto-pauses
/// the previous one. Status, list, and pause/resume commands work even when
/// `[goals] enabled = false` (read-only access is always available).
pub struct GoalCommand;

impl CommandHandler<CommandContext<'_>> for GoalCommand {
    fn name(&self) -> &'static str {
        "/goal"
    }

    fn description(&self) -> &'static str {
        "Manage long-horizon goals that persist across conversation turns"
    }

    fn args_hint(&self) -> &'static str {
        "create <text> [--budget N] | pause | resume | complete | clear | status | list"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Session
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
        let span = tracing::info_span!("commands.goal.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_goal(args).await?;
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
    fn goal_name_and_description() {
        assert_eq!(GoalCommand.name(), "/goal");
        assert!(!GoalCommand.description().is_empty());
    }

    #[tokio::test]
    async fn goal_propagates_error_from_agent() {
        // NullAgent::handle_goal returns Err — the handler must propagate it.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let result = GoalCommand.handle(&mut ctx, "status").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn goal_with_empty_args_propagates_error() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let result = GoalCommand.handle(&mut ctx, "").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn goal_dispatch_allowed_when_trusted() {
        // NullAgent::handle_goal errors, but the error must come from the handler,
        // not from the trust gate rejecting the dispatch.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(GoalCommand);

        let result = reg.dispatch(&mut ctx, "/goal status", true).await;
        let err = result.unwrap().unwrap_err();
        assert!(!err.0.contains("trusted"));
    }

    #[tokio::test]
    async fn goal_dispatch_rejected_when_untrusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(GoalCommand);

        let result = reg.dispatch(&mut ctx, "/goal status", false).await;
        let err = result.unwrap().unwrap_err();
        assert!(err.0.contains("trusted"));
    }
}
