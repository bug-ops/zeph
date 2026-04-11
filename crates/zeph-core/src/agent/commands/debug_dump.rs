// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `/debug-dump` slash command.

use std::future::Future;
use std::pin::Pin;

use crate::channel::Channel;

use super::super::command_registry::{
    CommandContext, CommandHandler, CommandOutput, SlashCategory,
};
use super::super::error::AgentError;

/// Enable or show the status of debug dump output.
///
/// With no arguments, reports whether debug dump is active and where.
/// With a path argument, enables debug dump to that directory.
pub(crate) struct DebugDumpCommand;

impl<C: Channel> CommandHandler<C> for DebugDumpCommand {
    fn name(&self) -> &'static str {
        "/debug-dump"
    }

    fn description(&self) -> &'static str {
        "Enable or toggle debug dump output"
    }

    fn args_hint(&self) -> &'static str {
        "[path]"
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
                let msg = match &ctx.debug_state.debug_dumper {
                    Some(d) => format!("Debug dump active: {}", d.dir().display()),
                    None => "Debug dump is inactive. Use `/debug-dump <path>` to enable, \
                         or start with `--debug-dump [dir]`."
                        .to_owned(),
                };
                return Ok(CommandOutput::Message(msg));
            }

            let dir = std::path::PathBuf::from(args);
            match crate::debug_dump::DebugDumper::new(&dir, ctx.debug_state.dump_format) {
                Ok(dumper) => {
                    let path = dumper.dir().display().to_string();
                    ctx.debug_state.debug_dumper = Some(dumper);
                    Ok(CommandOutput::Message(format!(
                        "Debug dump enabled: {path}"
                    )))
                }
                Err(e) => Ok(CommandOutput::Message(format!(
                    "Failed to enable debug dump: {e}"
                ))),
            }
        })
    }
}
