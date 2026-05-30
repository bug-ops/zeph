// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scheduler command handler: `/scheduler`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// List scheduled tasks.
///
/// Requires `scheduler` feature in `zeph-core`. Subcommands: (none or `list`).
pub struct SchedulerCommand;

impl CommandHandler<CommandContext<'_>> for SchedulerCommand {
    fn name(&self) -> &'static str {
        "/scheduler"
    }

    fn description(&self) -> &'static str {
        "List scheduled tasks"
    }

    fn args_hint(&self) -> &'static str {
        "[list]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("scheduler")
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
        let span = tracing::info_span!("commands.scheduler.handle");
        Box::pin(
            async move {
                if !args.is_empty() && args != "list" {
                    return Err(CommandError::new(
                        "Unknown /scheduler subcommand. Available: /scheduler list",
                    ));
                }
                match ctx.agent.list_scheduled_tasks().await? {
                    Some(msg) if msg.is_empty() => Ok(CommandOutput::Silent),
                    Some(msg) => Ok(CommandOutput::Message(msg)),
                    None => Ok(CommandOutput::Message(
                        "Scheduler is not enabled or list_tasks tool is unavailable.".to_owned(),
                    )),
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
    fn scheduler_name_and_description() {
        assert_eq!(SchedulerCommand.name(), "/scheduler");
        assert!(!SchedulerCommand.description().is_empty());
    }

    #[tokio::test]
    async fn scheduler_none_returns_not_enabled_message() {
        // NullAgent returns Ok(None), so the handler returns a "not enabled" message.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = SchedulerCommand.handle(&mut ctx, "").await.unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("not enabled") || msg.contains("unavailable"));
    }

    #[tokio::test]
    async fn scheduler_unknown_subcommand_returns_error() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let err = SchedulerCommand
            .handle(&mut ctx, "start")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Unknown") || err.to_string().contains("Available"));
    }
}
