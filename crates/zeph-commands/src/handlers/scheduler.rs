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

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            if !args.is_empty() && args != "list" {
                return Ok(CommandOutput::Message(
                    "Unknown /scheduler subcommand. Available: /scheduler list".to_owned(),
                ));
            }
            match ctx.agent.list_scheduled_tasks().await? {
                Some(msg) if msg.is_empty() => Ok(CommandOutput::Silent),
                Some(msg) => Ok(CommandOutput::Message(msg)),
                None => Ok(CommandOutput::Message(
                    "Scheduler is not enabled or list_tasks tool is unavailable.".to_owned(),
                )),
            }
        })
    }
}
