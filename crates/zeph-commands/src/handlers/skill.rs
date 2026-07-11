// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Skill command handlers: `/skill`, `/skills`, `/feedback`.
//!
//! These handlers delegate to `AgentAccess` methods which in turn call the
//! `_as_string` variants in `zeph-core`. The clone-before-await pattern in the
//! `AgentAccess` impl ensures the returned futures are `Send`-safe.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Load, manage, and create skills.
///
/// Subcommands: `stats`, `versions`, `activate`, `approve`, `reset`, `trust`,
/// `block`, `unblock`, `install`, `remove`, `create`, `scan`, `reject`.
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

    fn requires_auth(&self) -> bool {
        true
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.skill.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_skill(args).await?;
                Ok(CommandOutput::Message(result))
            }
            .instrument(span),
        )
    }
}

/// List loaded skills.
///
/// Subcommands: (none) list all; `confusability` show pairs with high embedding similarity.
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
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.skills.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_skills(args).await?;
                Ok(CommandOutput::Message(result))
            }
            .instrument(span),
        )
    }
}

/// Submit feedback for a skill invocation.
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

    fn requires_auth(&self) -> bool {
        true
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.feedback.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_feedback_command(args).await?;
                Ok(CommandOutput::Message(result))
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
    use std::assert_matches;

    #[test]
    fn skill_name_and_description() {
        assert_eq!(SkillCommand.name(), "/skill");
        assert!(!SkillCommand.description().is_empty());
    }

    #[test]
    fn skills_name_and_description() {
        assert_eq!(SkillsCommand.name(), "/skills");
        assert!(!SkillsCommand.description().is_empty());
    }

    #[test]
    fn feedback_name_and_description() {
        assert_eq!(FeedbackCommand.name(), "/feedback");
        assert!(!FeedbackCommand.description().is_empty());
    }

    #[tokio::test]
    async fn skill_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = SkillCommand.handle(&mut ctx, "stats").await.unwrap();
        assert_matches!(out, CommandOutput::Message(_));
    }

    #[tokio::test]
    async fn skills_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = SkillsCommand.handle(&mut ctx, "").await.unwrap();
        assert_matches!(out, CommandOutput::Message(_));
    }

    #[tokio::test]
    async fn feedback_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = FeedbackCommand
            .handle(&mut ctx, "my-skill good job")
            .await
            .unwrap();
        assert_matches!(out, CommandOutput::Message(_));
    }

    #[tokio::test]
    async fn skill_dispatch_allowed_when_trusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(SkillCommand);

        let result = reg.dispatch(&mut ctx, "/skill stats", true).await;
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn skill_dispatch_rejected_when_untrusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(SkillCommand);

        let result = reg.dispatch(&mut ctx, "/skill stats", false).await;
        let err = result.unwrap().unwrap_err();
        assert!(err.0.contains("trusted"));
    }

    #[tokio::test]
    async fn feedback_dispatch_allowed_when_trusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(FeedbackCommand);

        let result = reg
            .dispatch(&mut ctx, "/feedback my-skill good job", true)
            .await;
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn feedback_dispatch_rejected_when_untrusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(FeedbackCommand);

        let result = reg
            .dispatch(&mut ctx, "/feedback my-skill good job", false)
            .await;
        let err = result.unwrap().unwrap_err();
        assert!(err.0.contains("trusted"));
    }
}
