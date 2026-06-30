// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/trajectory` and `/scope` command handlers (spec 050 Phase 1).

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Inspect or reset the trajectory risk sentinel.
///
/// Subcommands: `status` (default), `reset`.
pub struct TrajectoryCommand;

impl CommandHandler<CommandContext<'_>> for TrajectoryCommand {
    fn name(&self) -> &'static str {
        "/trajectory"
    }

    fn description(&self) -> &'static str {
        "Show trajectory risk sentinel status or reset it"
    }

    fn args_hint(&self) -> &'static str {
        "[status|reset]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
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
        let span = tracing::info_span!("commands.trajectory.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_trajectory(args);
                Ok(CommandOutput::Message(result))
            }
            .instrument(span),
        )
    }
}

/// List configured capability scopes.
///
/// Subcommands: `list [task_type]` (default).
pub struct ScopeCommand;

impl CommandHandler<CommandContext<'_>> for ScopeCommand {
    fn name(&self) -> &'static str {
        "/scope"
    }

    fn description(&self) -> &'static str {
        "List configured capability scopes (spec 050)"
    }

    fn args_hint(&self) -> &'static str {
        "[list [task_type]]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
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
        let span = tracing::info_span!("commands.scope.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_scope(args);
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
    use std::assert_matches;

    #[test]
    fn trajectory_name_and_description() {
        assert_eq!(TrajectoryCommand.name(), "/trajectory");
        assert!(!TrajectoryCommand.description().is_empty());
    }

    #[test]
    fn scope_name_and_description() {
        assert_eq!(ScopeCommand.name(), "/scope");
        assert!(!ScopeCommand.description().is_empty());
    }

    #[tokio::test]
    async fn trajectory_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = TrajectoryCommand.handle(&mut ctx, "status").await.unwrap();
        assert_matches!(out, CommandOutput::Message(_));
    }

    #[tokio::test]
    async fn scope_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ScopeCommand.handle(&mut ctx, "list").await.unwrap();
        assert_matches!(out, CommandOutput::Message(_));
    }
}
