// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `/dump-format` slash command.

use std::future::Future;
use std::pin::Pin;

use crate::channel::Channel;

use super::super::command_registry::{
    CommandContext, CommandHandler, CommandOutput, SlashCategory,
};
use super::super::error::AgentError;

/// Switch debug dump format at runtime.
pub(crate) struct DumpFormatCommand;

impl<C: Channel> CommandHandler<C> for DumpFormatCommand {
    fn name(&self) -> &'static str {
        "/dump-format"
    }

    fn description(&self) -> &'static str {
        "Switch debug dump format at runtime"
    }

    fn args_hint(&self) -> &'static str {
        "<json|raw|trace>"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_, C>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, AgentError>> + Send + 'a>> {
        Box::pin(async move {
            if args.is_empty() {
                return Ok(CommandOutput::Message(format!(
                    "Current dump format: {:?}. Use `/dump-format json|raw|trace` to change.",
                    ctx.debug_state.dump_format
                )));
            }

            let new_format = match args {
                "json" => crate::debug_dump::DumpFormat::Json,
                "raw" => crate::debug_dump::DumpFormat::Raw,
                "trace" => crate::debug_dump::DumpFormat::Trace,
                other => {
                    return Ok(CommandOutput::Message(format!(
                        "Unknown format '{other}'. Valid values: json, raw, trace."
                    )));
                }
            };

            ctx.debug_state.switch_format(new_format);
            Ok(CommandOutput::Message(format!(
                "Debug dump format set to: {args}"
            )))
        })
    }
}
