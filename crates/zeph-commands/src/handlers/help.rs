// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/help` command handler.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Display all available slash commands grouped by category.
pub struct HelpCommand;

impl CommandHandler<CommandContext<'_>> for HelpCommand {
    fn name(&self) -> &'static str {
        "/help"
    }

    fn description(&self) -> &'static str {
        "Show this help message"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.help.handle");
        Box::pin(
            async move {
                let mut out = String::from("Slash commands:\n\n");

                let categories = [
                    SlashCategory::Session,
                    SlashCategory::Configuration,
                    SlashCategory::Memory,
                    SlashCategory::Skills,
                    SlashCategory::Planning,
                    SlashCategory::Integration,
                    SlashCategory::Debugging,
                    SlashCategory::Advanced,
                ];

                for cat in &categories {
                    let entries: Vec<_> = crate::COMMANDS
                        .iter()
                        .filter(|c| &c.category == cat)
                        .collect();
                    if entries.is_empty() {
                        continue;
                    }
                    let _ = writeln!(out, "{}:", cat.as_str());
                    for cmd in entries {
                        if cmd.args.is_empty() {
                            let _ = write!(out, "  {}", cmd.name);
                        } else {
                            let _ = write!(out, "  {} {}", cmd.name, cmd.args);
                        }
                        let _ = write!(out, "  — {}", cmd.description);
                        if let Some(feat) = cmd.feature_gate {
                            let _ = write!(out, " [requires: {feat}]");
                        }
                        let _ = writeln!(out);
                    }
                    let _ = writeln!(out);
                }

                Ok(CommandOutput::Message(out.trim_end().to_owned()))
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
    fn help_name_and_description() {
        assert_eq!(HelpCommand.name(), "/help");
        assert!(!HelpCommand.description().is_empty());
    }

    #[tokio::test]
    async fn help_returns_message_with_slash_commands_header() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = HelpCommand.handle(&mut ctx, "").await.unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("Slash commands"));
    }
}
