// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/goal` slash command handler.
//!
//! Subcommands:
//! - `create <text> [--budget N]` — create a new goal, pausing any existing active one
//! - `pause` — pause the active goal
//! - `resume` — resume the last paused goal
//! - `complete` — mark the active goal as completed
//! - `clear` — dismiss the active or paused goal
//! - `status` — show the active goal and recent history
//! - `list` — list all goals (active, paused, completed, cleared)

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Manage long-horizon goals that span multiple conversation turns.
///
/// At most one goal can be `active` at a time. Creating a new goal auto-pauses
/// the previous one. Status, list, and pause/resume commands work even when
/// `[goals] enabled = false` (read-only access is always available).
pub struct GoalCommand;

impl CommandHandler<CommandContext<'_>> for GoalCommand {
    fn name(&self) -> &'static str {
        "/goal"
    }

    fn description(&self) -> &'static str {
        "Manage long-horizon goals that persist across conversation turns"
    }

    fn args_hint(&self) -> &'static str {
        "create <text> [--budget N] | pause | resume | complete | clear | status | list"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Session
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.handle_goal(args).await.unwrap_or_else(|e| e.0);
            Ok(CommandOutput::Message(result))
        })
    }
}
