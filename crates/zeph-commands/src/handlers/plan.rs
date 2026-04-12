// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Planning handler: `/plan`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Planning handler stub.
///
/// This handler is NOT registered in any command registry. The `/plan` command is dispatched
/// directly via `dispatch_slash_command` because `dispatch_plan_command` produces a non-Send
/// future.
///
/// This struct exists to document the intended handler shape for when the Send constraint
/// is lifted in a future migration phase.
pub struct PlanCommand;

impl CommandHandler<CommandContext<'_>> for PlanCommand {
    fn name(&self) -> &'static str {
        "/plan"
    }

    fn description(&self) -> &'static str {
        "Create or manage execution plans"
    }

    fn args_hint(&self) -> &'static str {
        "[goal|confirm|cancel|status|list|resume|retry]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Planning
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            // Reconstruct the full command string so the plan parser can parse it.
            let input = if args.is_empty() {
                "/plan".to_owned()
            } else {
                format!("/plan {args}")
            };
            let result = ctx.agent.handle_plan(&input).await?;
            if result.is_empty() {
                Ok(CommandOutput::Silent)
            } else {
                Ok(CommandOutput::Message(result))
            }
        })
    }
}
