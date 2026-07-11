// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/undo` and `/redo` slash command handlers.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Undo the last N file-mutating shell commands executed in this session.
///
/// Syntax: `/undo [N]` — omit N to undo one step, or pass `list` to show the undo stack.
pub struct UndoCommand;

impl CommandHandler<CommandContext<'_>> for UndoCommand {
    fn name(&self) -> &'static str {
        "/undo"
    }

    fn description(&self) -> &'static str {
        "Undo the last N file-mutating shell commands (session-scoped)"
    }

    fn args_hint(&self) -> &'static str {
        "[N | list]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Session
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
        let span = tracing::info_span!("commands.undo.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_undo(args).await?;
                Ok(CommandOutput::Message(result))
            }
            .instrument(span),
        )
    }
}

/// Re-apply the last undone shell command.
///
/// Syntax: `/redo` — re-applies the most recently undone command.
pub struct RedoCommand;

impl CommandHandler<CommandContext<'_>> for RedoCommand {
    fn name(&self) -> &'static str {
        "/redo"
    }

    fn description(&self) -> &'static str {
        "Re-apply the last undone shell command"
    }

    fn args_hint(&self) -> &'static str {
        ""
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Session
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
        let span = tracing::info_span!("commands.redo.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_redo(args).await?;
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

    #[test]
    fn undo_name_and_description() {
        assert_eq!(UndoCommand.name(), "/undo");
        assert!(!UndoCommand.description().is_empty());
    }

    #[test]
    fn redo_name_and_description() {
        assert_eq!(RedoCommand.name(), "/redo");
        assert!(!RedoCommand.description().is_empty());
    }

    #[tokio::test]
    async fn undo_not_supported_returns_ok_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let result = UndoCommand.handle(&mut ctx, "").await;
        assert!(result.is_ok());
        if let Ok(CommandOutput::Message(msg)) = result {
            assert!(!msg.is_empty());
        }
    }

    #[tokio::test]
    async fn redo_not_supported_returns_ok_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let result = RedoCommand.handle(&mut ctx, "").await;
        assert!(result.is_ok());
        if let Ok(CommandOutput::Message(msg)) = result {
            assert!(!msg.is_empty());
        }
    }

    #[tokio::test]
    async fn undo_dispatch_rejected_when_untrusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(UndoCommand);

        let result = reg.dispatch(&mut ctx, "/undo", false).await;
        let err = result.unwrap().unwrap_err();
        assert!(err.0.contains("trusted"));
    }

    #[tokio::test]
    async fn undo_dispatch_allowed_when_trusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(UndoCommand);

        let result = reg.dispatch(&mut ctx, "/undo", true).await;
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn redo_dispatch_rejected_when_untrusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(RedoCommand);

        let result = reg.dispatch(&mut ctx, "/redo", false).await;
        let err = result.unwrap().unwrap_err();
        assert!(err.0.contains("trusted"));
    }

    #[tokio::test]
    async fn redo_dispatch_allowed_when_trusted() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(RedoCommand);

        let result = reg.dispatch(&mut ctx, "/redo", true).await;
        assert!(result.unwrap().is_ok());
    }
}
