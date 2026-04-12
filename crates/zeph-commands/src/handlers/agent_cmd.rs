// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sub-agent management handler: `/agent`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Manage sub-agents or dispatch `@mention` commands.
///
/// Delegates to `AgentAccess::handle_agent_dispatch`, which handles both `/agent`
/// subcommands and `@name` mentions. Returns `Continue` when the dispatch returns
/// `None` (no agent matched an `@mention` — fall through to LLM processing).
pub struct AgentCommand;

impl CommandHandler<CommandContext<'_>> for AgentCommand {
    fn name(&self) -> &'static str {
        "/agent"
    }

    fn description(&self) -> &'static str {
        "Manage sub-agents"
    }

    fn args_hint(&self) -> &'static str {
        "[subcommand]"
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
            let input = if args.is_empty() {
                "/agent".to_owned()
            } else {
                format!("/agent {args}")
            };
            match ctx.agent.handle_agent_dispatch(&input).await? {
                Some(msg) => Ok(CommandOutput::Message(msg)),
                None => Ok(CommandOutput::Silent),
            }
        })
    }
}
