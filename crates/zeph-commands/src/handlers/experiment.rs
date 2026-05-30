// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Experimental features handler: `/experiment`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Experimental features handler for `/experiment`.
///
/// Delegates to `AgentAccess::handle_experiment` which is now Send-compatible:
/// `handle_experiment_command_as_string` clones all `Arc` references before `.await`
/// so no `&mut self` borrow is held across await boundaries.
pub struct ExperimentCommand;

impl CommandHandler<CommandContext<'_>> for ExperimentCommand {
    fn name(&self) -> &'static str {
        "/experiment"
    }

    fn description(&self) -> &'static str {
        "Experimental features"
    }

    fn args_hint(&self) -> &'static str {
        "[subcommand]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("experiments")
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
        let span = tracing::info_span!("commands.experiment.handle");
        Box::pin(
            async move {
                let input = if args.is_empty() {
                    "/experiment".to_owned()
                } else {
                    format!("/experiment {args}")
                };
                let result = ctx.agent.handle_experiment(&input).await?;
                if result.is_empty() {
                    Ok(CommandOutput::Silent)
                } else {
                    Ok(CommandOutput::Message(result))
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

    #[test]
    fn experiment_name_and_description() {
        assert_eq!(ExperimentCommand.name(), "/experiment");
        assert!(!ExperimentCommand.description().is_empty());
    }

    #[tokio::test]
    async fn experiment_no_args_returns_silent_when_agent_returns_empty() {
        // NullAgent returns empty string from handle_experiment, so result is Silent.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ExperimentCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Silent));
    }

    #[tokio::test]
    async fn experiment_with_args_returns_silent_when_agent_returns_empty() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ExperimentCommand.handle(&mut ctx, "list").await.unwrap();
        assert!(matches!(out, CommandOutput::Silent));
    }
}
