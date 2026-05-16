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
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.goal.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_goal(args).await.unwrap_or_else(|e| e.0);
                Ok(CommandOutput::Message(result))
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::CommandContext;
    use crate::sink::NullSink;
    use crate::traits::debug::DebugAccess;
    use crate::traits::messages::MessageAccess;
    use crate::traits::session::SessionAccess;
    use std::future::Future;
    use std::pin::Pin;

    struct MockDebug;
    impl DebugAccess for MockDebug {
        fn log_status(&self) -> String {
            String::new()
        }
        fn read_log_tail<'a>(
            &'a self,
            _n: usize,
        ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
            Box::pin(async { None })
        }
        fn scrub(&self, text: &str) -> String {
            text.to_owned()
        }
        fn dump_status(&self) -> Option<String> {
            None
        }
        fn dump_format_name(&self) -> String {
            String::new()
        }
        fn enable_dump(&mut self, _dir: &str) -> Result<String, CommandError> {
            Ok(String::new())
        }
        fn set_dump_format(&mut self, _name: &str) -> Result<(), CommandError> {
            Ok(())
        }
    }

    struct MockMessages;
    impl MessageAccess for MockMessages {
        fn clear_history(&mut self) {}
        fn queue_len(&self) -> usize {
            0
        }
        fn drain_queue(&mut self) -> usize {
            0
        }
        fn notify_queue_count<'a>(
            &'a mut self,
            _count: usize,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
    }

    struct MockSession;
    impl SessionAccess for MockSession {
        fn supports_exit(&self) -> bool {
            false
        }
    }

    fn make_ctx<'a>(
        sink: &'a mut NullSink,
        debug: &'a mut MockDebug,
        messages: &'a mut MockMessages,
        session: &'a MockSession,
        agent: &'a mut crate::NullAgent,
    ) -> CommandContext<'a> {
        CommandContext {
            sink,
            debug,
            messages,
            session: session as &dyn SessionAccess,
            agent,
        }
    }

    #[test]
    fn goal_name_and_description() {
        assert_eq!(GoalCommand.name(), "/goal");
        assert!(!GoalCommand.description().is_empty());
    }

    #[tokio::test]
    async fn goal_returns_message_when_agent_errors() {
        // NullAgent default handle_goal returns Err, which is unwrapped to the error message.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = GoalCommand.handle(&mut ctx, "status").await.unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }

    #[tokio::test]
    async fn goal_with_empty_args_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = GoalCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }
}
