// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Status display handlers: `/status`, `/guardrail`, `/focus`, `/sidequest`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Display the current session status (provider, model, tokens, uptime, etc.).
pub struct StatusCommand;

impl CommandHandler<CommandContext<'_>> for StatusCommand {
    fn name(&self) -> &'static str {
        "/status"
    }

    fn description(&self) -> &'static str {
        "Show current session status (provider, model, tokens, uptime)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.session_status().await?;
            Ok(CommandOutput::Message(result))
        })
    }
}

/// Display guardrail configuration and runtime statistics.
pub struct GuardrailCommand;

impl CommandHandler<CommandContext<'_>> for GuardrailCommand {
    fn name(&self) -> &'static str {
        "/guardrail"
    }

    fn description(&self) -> &'static str {
        "Show guardrail status (provider, model, action, timeout, stats)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("guardrail")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.guardrail_status();
            Ok(CommandOutput::Message(result))
        })
    }
}

/// Display Focus Agent status (active session, knowledge block size).
pub struct FocusCommand;

impl CommandHandler<CommandContext<'_>> for FocusCommand {
    fn name(&self) -> &'static str {
        "/focus"
    }

    fn description(&self) -> &'static str {
        "Show Focus Agent status (active session, knowledge block size)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("context-compression")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.focus_status();
            Ok(CommandOutput::Message(result))
        })
    }
}

/// Display `SideQuest` eviction statistics (passes run, tokens freed).
pub struct SideQuestCommand;

impl CommandHandler<CommandContext<'_>> for SideQuestCommand {
    fn name(&self) -> &'static str {
        "/sidequest"
    }

    fn description(&self) -> &'static str {
        "Show SideQuest eviction stats (passes run, tokens freed)"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("context-compression")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = ctx.agent.sidequest_status();
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
    fn status_name_and_description() {
        assert_eq!(StatusCommand.name(), "/status");
        assert!(!StatusCommand.description().is_empty());
    }

    #[test]
    fn guardrail_name_and_description() {
        assert_eq!(GuardrailCommand.name(), "/guardrail");
        assert!(!GuardrailCommand.description().is_empty());
    }

    #[test]
    fn focus_name_and_description() {
        assert_eq!(FocusCommand.name(), "/focus");
        assert!(!FocusCommand.description().is_empty());
    }

    #[test]
    fn sidequest_name_and_description() {
        assert_eq!(SideQuestCommand.name(), "/sidequest");
        assert!(!SideQuestCommand.description().is_empty());
    }

    #[tokio::test]
    async fn status_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = StatusCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }

    #[tokio::test]
    async fn guardrail_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = GuardrailCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }

    #[tokio::test]
    async fn focus_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = FocusCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }

    #[tokio::test]
    async fn sidequest_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = SideQuestCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }
}
