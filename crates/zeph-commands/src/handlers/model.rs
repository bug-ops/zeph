// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Model and provider command handlers: `/model`, `/provider`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Show or switch the active LLM model.
///
/// - `/model` — list available models.
/// - `/model refresh` — clear cache and re-fetch from remote.
/// - `/model <id>` — switch to the given model.
pub struct ModelCommand;

impl CommandHandler<CommandContext<'_>> for ModelCommand {
    fn name(&self) -> &'static str {
        "/model"
    }

    fn description(&self) -> &'static str {
        "Show or switch the active model"
    }

    fn args_hint(&self) -> &'static str {
        "[id|refresh]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Configuration
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.model.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_model(args).await;
                if result.is_empty() {
                    Ok(CommandOutput::Silent)
                } else {
                    Ok(CommandOutput::Message(result))
                }
            }
            .instrument(span),
        )
    }
}

/// List configured providers or switch to one by name.
///
/// - `/provider` or `/provider status` — list all configured providers and their status.
/// - `/provider <name>` — switch to the named provider.
pub struct ProviderCommand;

impl CommandHandler<CommandContext<'_>> for ProviderCommand {
    fn name(&self) -> &'static str {
        "/provider"
    }

    fn description(&self) -> &'static str {
        "List configured providers or switch to one by name"
    }

    fn args_hint(&self) -> &'static str {
        "[name|status]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Configuration
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.provider.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_provider(args).await;
                if result.is_empty() {
                    Ok(CommandOutput::Silent)
                } else {
                    Ok(CommandOutput::Message(result))
                }
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
    fn model_name_and_description() {
        assert_eq!(ModelCommand.name(), "/model");
        assert!(!ModelCommand.description().is_empty());
    }

    #[test]
    fn provider_name_and_description() {
        assert_eq!(ProviderCommand.name(), "/provider");
        assert!(!ProviderCommand.description().is_empty());
    }

    #[tokio::test]
    async fn model_returns_silent_when_agent_returns_empty() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ModelCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Silent));
    }

    #[tokio::test]
    async fn provider_returns_silent_when_agent_returns_empty() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ProviderCommand.handle(&mut ctx, "status").await.unwrap();
        assert!(matches!(out, CommandOutput::Silent));
    }
}
