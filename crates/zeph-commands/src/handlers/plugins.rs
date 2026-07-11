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

    fn requires_auth(&self) -> bool {
        true
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.plugins.handle");
        Box::pin(
            async move {
                let msg = ctx.agent.handle_plugins(args).await?;
                Ok(CommandOutput::message_or_silent(msg))
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandRegistry;
    use crate::handlers::test_helpers::{MockDebug, MockMessages, MockSession, make_ctx};
    use crate::sink::NullSink;

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

    #[tokio::test]
    async fn plugins_dispatch_allowed_when_trusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(PluginsCommand);

        let result = reg.dispatch(&mut ctx, "/plugins list", true).await;
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn plugins_dispatch_rejected_when_untrusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(PluginsCommand);

        let result = reg
            .dispatch(&mut ctx, "/plugins add /etc/passwd", false)
            .await;
        let err = result.unwrap().unwrap_err();
        assert!(err.0.contains("trusted"));
    }
}
