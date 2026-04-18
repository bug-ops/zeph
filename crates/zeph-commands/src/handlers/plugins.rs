// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/plugins` slash command handler.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Manage installed plugins (list, install, remove, update).
pub struct PluginsCommand;

impl CommandHandler<CommandContext<'_>> for PluginsCommand {
    fn name(&self) -> &'static str {
        "/plugins"
    }

    fn description(&self) -> &'static str {
        "Manage installed plugins (list, install, remove, update)"
    }

    fn args_hint(&self) -> &'static str {
        "[list | install <name> | remove <name> | update [name]]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Integration
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            match ctx.agent.handle_plugins(args).await? {
                msg if msg.is_empty() => Ok(CommandOutput::Silent),
                msg => Ok(CommandOutput::Message(msg)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_slash_plugins() {
        assert_eq!(PluginsCommand.name(), "/plugins");
    }

    #[test]
    fn category_is_integration() {
        assert_eq!(PluginsCommand.category(), SlashCategory::Integration);
    }

    #[test]
    fn description_is_non_empty() {
        assert!(!PluginsCommand.description().is_empty());
    }

    #[test]
    fn args_hint_is_non_empty() {
        assert!(!PluginsCommand.args_hint().is_empty());
    }
}
