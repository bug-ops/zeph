// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Worktree command handler: `/worktree`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// List or clean the live session's git worktrees.
///
/// Subcommands: `list` (default) or `clean [--force]`. Reflects the running agent's actual
/// worktree state rather than a fresh disk scan (contrast with the CLI's
/// `zeph worktree list`/`clean`).
pub struct WorktreeCommand;

impl CommandHandler<CommandContext<'_>> for WorktreeCommand {
    fn name(&self) -> &'static str {
        "/worktree"
    }

    fn description(&self) -> &'static str {
        "List or clean the live session's git worktrees"
    }

    fn args_hint(&self) -> &'static str {
        "list | clean [--force]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
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
        let span = tracing::info_span!("commands.worktree.handle");
        Box::pin(
            async move {
                let words: Vec<&str> = args.split_whitespace().collect();
                let result = match words.as_slice() {
                    [] | ["list"] => ctx.agent.list_worktrees().await?,
                    ["clean"] => ctx.agent.clean_worktrees(false).await?,
                    ["clean", "--force"] => ctx.agent.clean_worktrees(true).await?,
                    _ => {
                        return Err(CommandError::new(
                            "Unknown /worktree subcommand. Available: /worktree list, \
                             /worktree clean [--force]",
                        ));
                    }
                };
                match result {
                    Some(msg) => Ok(CommandOutput::Message(msg)),
                    None => Ok(CommandOutput::Message(
                        "Worktree subsystem is not enabled for this session.".to_owned(),
                    )),
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
    fn worktree_name_and_description() {
        assert_eq!(WorktreeCommand.name(), "/worktree");
        assert!(!WorktreeCommand.description().is_empty());
    }

    #[tokio::test]
    async fn worktree_none_returns_not_enabled_message() {
        // NullAgent returns Ok(None), so the handler returns a "not enabled" message.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = WorktreeCommand.handle(&mut ctx, "").await.unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("not enabled"));
    }

    #[tokio::test]
    async fn worktree_list_subcommand_returns_not_enabled_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = WorktreeCommand.handle(&mut ctx, "list").await.unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("not enabled"));
    }

    #[tokio::test]
    async fn worktree_clean_force_subcommand_returns_not_enabled_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = WorktreeCommand
            .handle(&mut ctx, "clean --force")
            .await
            .unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("not enabled"));
    }

    #[tokio::test]
    async fn worktree_clean_force_tolerates_irregular_whitespace() {
        // Regression: exact-string matching on "clean --force" broke on double spaces or
        // leading/trailing whitespace; split_whitespace tokenization must not.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = WorktreeCommand
            .handle(&mut ctx, "  clean   --force  ")
            .await
            .unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("not enabled"));
    }

    #[tokio::test]
    async fn worktree_unknown_subcommand_returns_error() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let err = WorktreeCommand.handle(&mut ctx, "bogus").await.unwrap_err();
        assert!(err.to_string().contains("Unknown"));
    }
}
