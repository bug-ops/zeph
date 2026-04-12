// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Status display handlers: `/status`, `/guardrail`, `/focus`, `/sidequest`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Display the current session status (provider, model, tokens, uptime, etc.).
pub struct StatusCommand;

impl CommandHandler<CommandContext<'_>> for StatusCommand {
    fn name(&self) -> &'static str {
        "/status"
    }

    fn description(&self) -> &'static str {
        "Show current session status (provider, model, tokens, uptime)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.session_status().await?;
            Ok(CommandOutput::Message(result))
        })
    }
}

/// Display guardrail configuration and runtime statistics.
pub struct GuardrailCommand;

impl CommandHandler<CommandContext<'_>> for GuardrailCommand {
    fn name(&self) -> &'static str {
        "/guardrail"
    }

    fn description(&self) -> &'static str {
        "Show guardrail status (provider, model, action, timeout, stats)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("guardrail")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.guardrail_status();
            Ok(CommandOutput::Message(result))
        })
    }
}

/// Display Focus Agent status (active session, knowledge block size).
pub struct FocusCommand;

impl CommandHandler<CommandContext<'_>> for FocusCommand {
    fn name(&self) -> &'static str {
        "/focus"
    }

    fn description(&self) -> &'static str {
        "Show Focus Agent status (active session, knowledge block size)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("context-compression")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.focus_status();
            Ok(CommandOutput::Message(result))
        })
    }
}

/// Display `SideQuest` eviction statistics (passes run, tokens freed).
pub struct SideQuestCommand;

impl CommandHandler<CommandContext<'_>> for SideQuestCommand {
    fn name(&self) -> &'static str {
        "/sidequest"
    }

    fn description(&self) -> &'static str {
        "Show SideQuest eviction stats (passes run, tokens freed)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("context-compression")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.sidequest_status();
            Ok(CommandOutput::Message(result))
        })
    }
}
