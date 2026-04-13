// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Conversation management handlers: `/new` and `/compact`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Compact context handler for `/compact`.
///
/// Delegates to `AgentAccess::compact_context`. The implementation extracts all
/// non-`Send` borrows before `.await` points so the future satisfies `Send + 'a`.
pub struct CompactCommand;

impl CommandHandler<CommandContext<'_>> for CompactCommand {
    fn name(&self) -> &'static str {
        "/compact"
    }

    fn description(&self) -> &'static str {
        "Compact the context window by summarizing older messages"
    }

    fn args_hint(&self) -> &'static str {
        ""
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Session
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.compact_context().await?;
            Ok(CommandOutput::Message(result))
        })
    }
}

/// New conversation handler for `/new`.
///
/// Delegates to `AgentAccess::reset_conversation` which is now Send-compatible:
/// `reset_conversation` clones the `Arc<SemanticMemory>` before `.await` so no
/// `&mut self` borrow is held across the await boundary.
pub struct NewConversationCommand;

impl CommandHandler<CommandContext<'_>> for NewConversationCommand {
    fn name(&self) -> &'static str {
        "/new"
    }

    fn description(&self) -> &'static str {
        "Start a new conversation (reset context, preserve memory and MCP)"
    }

    fn args_hint(&self) -> &'static str {
        "[--no-digest] [--keep-plan]"
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
            let (keep_plan, no_digest) = parse_new_flags(args);
            let result = ctx.agent.reset_conversation(keep_plan, no_digest).await?;
            Ok(CommandOutput::Message(result))
        })
    }
}

/// Parse `--keep-plan` and `--no-digest` flags from the `/new` command args string.
fn parse_new_flags(args: &str) -> (bool, bool) {
    let keep_plan = args.split_whitespace().any(|a| a == "--keep-plan");
    let no_digest = args.split_whitespace().any(|a| a == "--no-digest");
    (keep_plan, no_digest)
}

#[cfg(test)]
mod tests {
    use super::parse_new_flags;

    #[test]
    fn no_flags_both_false() {
        assert_eq!(parse_new_flags(""), (false, false));
        assert_eq!(parse_new_flags("   "), (false, false));
    }

    #[test]
    fn keep_plan_flag_detected() {
        assert_eq!(parse_new_flags("--keep-plan"), (true, false));
        assert_eq!(parse_new_flags("--keep-plan --no-digest"), (true, true));
    }

    #[test]
    fn no_digest_flag_detected() {
        assert_eq!(parse_new_flags("--no-digest"), (false, true));
    }

    #[test]
    fn both_flags_order_independent() {
        assert_eq!(parse_new_flags("--no-digest --keep-plan"), (true, true));
        assert_eq!(parse_new_flags("--keep-plan --no-digest"), (true, true));
    }

    #[test]
    fn partial_flag_name_not_matched() {
        assert_eq!(parse_new_flags("--keep"), (false, false));
        assert_eq!(parse_new_flags("--no"), (false, false));
        assert_eq!(parse_new_flags("keep-plan"), (false, false));
    }
}
