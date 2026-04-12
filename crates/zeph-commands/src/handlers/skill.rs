// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Skill command stubs: `/skill`, `/skills`, `/feedback`.
//!
//! These handlers are registered so the command registry recognises the slash
//! commands and can surface them in help/completion output, but the actual
//! dispatch is intentionally left to `handle_builtin_command` in `zeph-core`.
//!
//! # Why not implement them here?
//!
//! `handle_skill_command_as_string`, `handle_skills_family_as_string`, and
//! `handle_feedback_as_string` hold `&SemanticMemory` and `&AnyProvider`
//! references across `.await` points.  `&'a T: Send` requires `T: Sync`, but
//! neither `SemanticMemory` nor `AnyProvider` implement `Sync`.  Adding them to
//! `AgentAccess` would therefore break the `Send` bound that object-safe trait
//! dispatch requires.  The migration is deferred until those types implement
//! `Sync`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Load, manage, and create skills.
///
/// Subcommands: `stats`, `versions`, `activate`, `approve`, `reset`, `trust`,
/// `block`, `unblock`, `install`, `remove`, `create`, `scan`, `reject`.
///
/// Actual dispatch is handled by `handle_builtin_command` in `zeph-core`; this
/// stub exists so the registry exposes `/skill` in help and completion lists.
pub struct SkillCommand;

impl CommandHandler<CommandContext<'_>> for SkillCommand {
    fn name(&self) -> &'static str {
        "/skill"
    }

    fn description(&self) -> &'static str {
        "Load and display a skill body, or manage skill lifecycle"
    }

    fn args_hint(&self) -> &'static str {
        "<name|subcommand>"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Skills
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        // Deferred: dispatched via handle_builtin_command in zeph-core.
        Box::pin(async move { Ok(CommandOutput::Silent) })
    }
}

/// List loaded skills.
///
/// Subcommands: (none) list all; `confusability` show pairs with high embedding
/// similarity.
///
/// Actual dispatch is handled by `handle_builtin_command` in `zeph-core`.
pub struct SkillsCommand;

impl CommandHandler<CommandContext<'_>> for SkillsCommand {
    fn name(&self) -> &'static str {
        "/skills"
    }

    fn description(&self) -> &'static str {
        "List loaded skills (grouped by category when available)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Skills
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        // Deferred: dispatched via handle_builtin_command in zeph-core.
        Box::pin(async move { Ok(CommandOutput::Silent) })
    }
}

/// Submit feedback for a skill invocation.
///
/// Actual dispatch is handled by `handle_builtin_command` in `zeph-core`.
pub struct FeedbackCommand;

impl CommandHandler<CommandContext<'_>> for FeedbackCommand {
    fn name(&self) -> &'static str {
        "/feedback"
    }

    fn description(&self) -> &'static str {
        "Submit feedback for a skill"
    }

    fn args_hint(&self) -> &'static str {
        "<skill> <message>"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Skills
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        // Deferred: dispatched via handle_builtin_command in zeph-core.
        Box::pin(async move { Ok(CommandOutput::Silent) })
    }
}
