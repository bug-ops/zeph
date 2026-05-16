// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scheduler command handler: `/scheduler`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// List scheduled tasks.
///
/// Requires `scheduler` feature in `zeph-core`. Subcommands: (none or `list`).
pub struct SchedulerCommand;

impl CommandHandler<CommandContext<'_>> for SchedulerCommand {
    fn name(&self) -> &'static str {
        "/scheduler"
    }

    fn description(&self) -> &'static str {
        "List scheduled tasks"
    }

    fn args_hint(&self) -> &'static str {
        "[list]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("scheduler")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            if !args.is_empty() && args != "list" {
                return Ok(CommandOutput::Message(
                    "Unknown /scheduler subcommand. Available: /scheduler list".to_owned(),
                ));
            }
            match ctx.agent.list_scheduled_tasks().await? {
                Some(msg) if msg.is_empty() => Ok(CommandOutput::Silent),
                Some(msg) => Ok(CommandOutput::Message(msg)),
                None => Ok(CommandOutput::Message(
                    "Scheduler is not enabled or list_tasks tool is unavailable.".to_owned(),
                )),
            }
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
    fn scheduler_name_and_description() {
        assert_eq!(SchedulerCommand.name(), "/scheduler");
        assert!(!SchedulerCommand.description().is_empty());
    }

    #[tokio::test]
    async fn scheduler_none_returns_not_enabled_message() {
        // NullAgent returns Ok(None), so the handler returns a "not enabled" message.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = SchedulerCommand.handle(&mut ctx, "").await.unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("not enabled") || msg.contains("unavailable"));
    }

    #[tokio::test]
    async fn scheduler_unknown_subcommand_returns_error_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = SchedulerCommand.handle(&mut ctx, "start").await.unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("Unknown") || msg.contains("Available"));
    }
}
