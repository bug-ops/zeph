// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for `/log` slash command.

use std::future::Future;
use std::pin::Pin;

use crate::channel::Channel;

use super::super::command_registry::{
    CommandContext, CommandHandler, CommandOutput, SlashCategory,
};
use super::super::error::AgentError;
use super::super::log_commands;

/// Show log file path and recent log entries.
pub(crate) struct LogCommand;

impl<C: Channel> CommandHandler<C> for LogCommand {
    fn name(&self) -> &'static str {
        "/log"
    }

    fn description(&self) -> &'static str {
        "Toggle verbose log output"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_, C>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, AgentError>> + Send + 'a>> {
        Box::pin(async move {
            use std::fmt::Write as _;

            let logging = ctx.debug_state.logging_config.clone();
            let mut out = String::new();
            log_commands::format_logging_status(&logging, &mut out);

            if !logging.file.is_empty() {
                let base_path = std::path::PathBuf::from(&logging.file);
                let tail = tokio::task::spawn_blocking(move || {
                    let actual = log_commands::resolve_current_log_file(&base_path);
                    actual.and_then(|p| log_commands::read_log_tail(&p, 20))
                })
                .await
                .unwrap_or(None);

                if let Some(lines) = tail {
                    let _ = writeln!(out);
                    let _ = writeln!(out, "Recent entries:");
                    out.push_str(&crate::redact::scrub_content(&lines));
                }
            }

            Ok(CommandOutput::Message(out.trim_end().to_owned()))
        })
    }
}
