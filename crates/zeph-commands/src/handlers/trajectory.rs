// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/trajectory` and `/scope` command handlers (spec 050 Phase 1).

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Inspect or reset the trajectory risk sentinel.
///
/// Subcommands: `status` (default), `reset`.
pub struct TrajectoryCommand;

impl CommandHandler<CommandContext<'_>> for TrajectoryCommand {
    fn name(&self) -> &'static str {
        "/trajectory"
    }

    fn description(&self) -> &'static str {
        "Show trajectory risk sentinel status or reset it"
    }

    fn args_hint(&self) -> &'static str {
        "[status|reset]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.handle_trajectory(args);
            Ok(CommandOutput::Message(result))
        })
    }
}

/// List configured capability scopes.
///
/// Subcommands: `list [task_type]` (default).
pub struct ScopeCommand;

impl CommandHandler<CommandContext<'_>> for ScopeCommand {
    fn name(&self) -> &'static str {
        "/scope"
    }

    fn description(&self) -> &'static str {
        "List configured capability scopes (spec 050)"
    }

    fn args_hint(&self) -> &'static str {
        "[list [task_type]]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.handle_scope(args);
            Ok(CommandOutput::Message(result))
        })
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
    fn trajectory_name_and_description() {
        assert_eq!(TrajectoryCommand.name(), "/trajectory");
        assert!(!TrajectoryCommand.description().is_empty());
    }

    #[test]
    fn scope_name_and_description() {
        assert_eq!(ScopeCommand.name(), "/scope");
        assert!(!ScopeCommand.description().is_empty());
    }

    #[tokio::test]
    async fn trajectory_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = TrajectoryCommand.handle(&mut ctx, "status").await.unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }

    #[tokio::test]
    async fn scope_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ScopeCommand.handle(&mut ctx, "list").await.unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }
}
