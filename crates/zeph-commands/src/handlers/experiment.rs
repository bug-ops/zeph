// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Experimental features handler: `/experiment`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Experimental features handler stub.
///
/// This handler is NOT registered in any command registry. The `/experiment` command is
/// dispatched directly via `dispatch_slash_command` because `handle_experiment_command`
/// produces a non-Send future.
///
/// This struct exists to document the intended handler shape for when the Send constraint
/// is lifted in a future migration phase.
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
