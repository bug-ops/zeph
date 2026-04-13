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

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
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
        })
    }
}
