// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Policy command handler: `/policy`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Inspect policy status or dry-run evaluation.
///
/// Subcommands: `status` (default), `check <tool> [args_json]`.
pub struct PolicyCommand;

impl CommandHandler<CommandContext<'_>> for PolicyCommand {
    fn name(&self) -> &'static str {
        "/policy"
    }

    fn description(&self) -> &'static str {
        "Inspect policy status or dry-run evaluation"
    }

    fn args_hint(&self) -> &'static str {
        "[status|check <tool> [args_json]]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("policy-enforcer")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.handle_policy(args).await?;
            if result.is_empty() {
                Ok(CommandOutput::Silent)
            } else {
                Ok(CommandOutput::Message(result))
            }
        })
    }
}
