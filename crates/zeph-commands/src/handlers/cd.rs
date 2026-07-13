// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Working-directory switch command: `/cd`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Switch the session's primary working directory, or report the current one (#6032).
///
/// User-facing entry point into the same mechanism the LLM-invoked `set_working_directory`
/// tool already uses — reachable identically from CLI, TUI, and ACP via the shared slash
/// dispatch path. Conversation history, active goals, and skill state are preserved; only
/// cwd-derived state (file-tool root, repo-map, and — unless `--safe-mode` is active —
/// CLAUDE.md/AGENTS.md instructions) is affected.
pub struct CdCommand;

impl CommandHandler<CommandContext<'_>> for CdCommand {
    fn name(&self) -> &'static str {
        "/cd"
    }

    fn description(&self) -> &'static str {
        "Change the session's working directory (no arg: show current)"
    }

    fn args_hint(&self) -> &'static str {
        "[path]"
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
        // No handler-level span here: `change_working_directory` already opens
        // `core.commands.cd` per NFR-004 — a second nested `commands.cd.handle` span around
        // this thin delegation would be redundant tracing overhead.
        Box::pin(async move {
            let result = ctx.agent.change_working_directory(args).await?;
            Ok(CommandOutput::Message(result))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::test_helpers::{MockDebug, MockMessages, MockSession, make_ctx};
    use crate::sink::NullSink;

    #[test]
    fn cd_name_and_description() {
        assert_eq!(CdCommand.name(), "/cd");
        assert!(!CdCommand.description().is_empty());
    }

    #[test]
    fn cd_requires_auth() {
        assert!(CdCommand.requires_auth());
    }

    #[tokio::test]
    async fn cd_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        // NullAgent's default change_working_directory returns an error — verifies the
        // handler propagates it rather than swallowing it.
        let out = CdCommand.handle(&mut ctx, "").await;
        assert!(out.is_err());
    }
}
