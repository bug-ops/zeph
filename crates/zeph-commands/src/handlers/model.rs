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
        Box::pin(async move {
            let result = ctx.agent.handle_model(args).await;
            if result.is_empty() {
                Ok(CommandOutput::Silent)
            } else {
                Ok(CommandOutput::Message(result))
            }
        })
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
        Box::pin(async move {
            let result = ctx.agent.handle_provider(args).await;
            if result.is_empty() {
                Ok(CommandOutput::Silent)
            } else {
                Ok(CommandOutput::Message(result))
            }
        })
    }
}
