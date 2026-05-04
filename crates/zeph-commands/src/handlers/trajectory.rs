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

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.handle_trajectory(args);
            Ok(CommandOutput::Message(result))
        })
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

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.handle_scope(args);
            Ok(CommandOutput::Message(result))
        })
    }
}
