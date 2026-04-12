// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session management command handlers: `/exit`, `/quit`, `/clear`, `/reset`, `/clear-queue`.

use std::future::Future;
use std::pin::Pin;

use crate::CommandHandler;
use crate::context::CommandContext;
use crate::{CommandError, CommandOutput, SlashCategory};

/// Exit the agent loop.
///
/// `/exit` and `/quit` are treated as aliases; both map to this handler via the registry.
/// When the channel does not support exit (e.g., Telegram), the command is rejected with
/// a user-visible message.
pub struct ExitCommand;

impl CommandHandler<CommandContext<'_>> for ExitCommand {
    fn name(&self) -> &'static str {
        "/exit"
    }

    fn description(&self) -> &'static str {
        "Exit the agent (also: /quit)"
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
            if ctx.session.supports_exit() {
                Ok(CommandOutput::Exit)
            } else {
                ctx.sink
                    .send("/exit is not supported in this channel.")
                    .await?;
                Ok(CommandOutput::Continue)
            }
        })
    }
}

/// Alias for `/exit`.
pub struct QuitCommand;

impl CommandHandler<CommandContext<'_>> for QuitCommand {
    fn name(&self) -> &'static str {
        "/quit"
    }

    fn description(&self) -> &'static str {
        "Exit the agent (alias for /exit)"
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
            if ctx.session.supports_exit() {
                Ok(CommandOutput::Exit)
            } else {
                ctx.sink
                    .send("/exit is not supported in this channel.")
                    .await?;
                Ok(CommandOutput::Continue)
            }
        })
    }
}

/// Clear conversation history and tool caches without sending a confirmation message.
///
/// Clears the message history (keeping only the system prompt), tool caches,
/// pending images, and URL tracking.
pub struct ClearCommand;

impl CommandHandler<CommandContext<'_>> for ClearCommand {
    fn name(&self) -> &'static str {
        "/clear"
    }

    fn description(&self) -> &'static str {
        "Clear conversation history"
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
            ctx.messages.clear_history();
            Ok(CommandOutput::Silent)
        })
    }
}

/// Reset conversation history (alias for `/clear`, replies with confirmation).
pub struct ResetCommand;

impl CommandHandler<CommandContext<'_>> for ResetCommand {
    fn name(&self) -> &'static str {
        "/reset"
    }

    fn description(&self) -> &'static str {
        "Reset conversation history (alias for /clear, replies with confirmation)"
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
            ctx.messages.clear_history();
            Ok(CommandOutput::Message(
                "Conversation history reset.".to_owned(),
            ))
        })
    }
}

/// Discard all messages currently queued for processing.
pub struct ClearQueueCommand;

impl CommandHandler<CommandContext<'_>> for ClearQueueCommand {
    fn name(&self) -> &'static str {
        "/clear-queue"
    }

    fn description(&self) -> &'static str {
        "Discard queued messages"
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
            let n = ctx.messages.drain_queue();
            // Notify the channel of the updated count; ignore errors (best-effort).
            let _ = ctx.sink.send_queue_count(0).await;
            Ok(CommandOutput::Message(format!(
                "Cleared {n} queued messages."
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandRegistry;
    use crate::context::CommandContext;
    use crate::sink::ChannelSink;
    use crate::traits::debug::DebugAccess;
    use crate::traits::messages::MessageAccess;
    use crate::traits::session::SessionAccess;
    use std::future::Future;
    use std::pin::Pin;

    // --- Mock implementations ---

    struct MockSink {
        sent: Vec<String>,
    }

    impl ChannelSink for MockSink {
        fn send<'a>(
            &'a mut self,
            msg: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), CommandError>> + Send + 'a>> {
            self.sent.push(msg.to_owned());
            Box::pin(async { Ok(()) })
        }

        fn flush_chunks<'a>(
            &'a mut self,
        ) -> Pin<Box<dyn Future<Output = Result<(), CommandError>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn send_queue_count<'a>(
            &'a mut self,
            _count: usize,
        ) -> Pin<Box<dyn Future<Output = Result<(), CommandError>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn supports_exit(&self) -> bool {
            false
        }
    }

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
            "raw".to_owned()
        }

        fn enable_dump(&mut self, _dir: &str) -> Result<String, CommandError> {
            Ok("/tmp".to_owned())
        }

        fn set_dump_format(&mut self, _name: &str) -> Result<(), CommandError> {
            Ok(())
        }
    }

    struct MockMessages {
        pub cleared: bool,
        pub queue: usize,
    }

    impl MessageAccess for MockMessages {
        fn clear_history(&mut self) {
            self.cleared = true;
        }

        fn queue_len(&self) -> usize {
            self.queue
        }

        fn drain_queue(&mut self) -> usize {
            let n = self.queue;
            self.queue = 0;
            n
        }

        fn notify_queue_count<'a>(
            &'a mut self,
            _count: usize,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
    }

    struct MockSession {
        supports_exit: bool,
    }

    impl SessionAccess for MockSession {
        fn supports_exit(&self) -> bool {
            self.supports_exit
        }
    }

    fn make_ctx<'a>(
        sink: &'a mut MockSink,
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

    // --- Tests ---

    #[tokio::test]
    async fn exit_returns_exit_when_supported() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
        };
        let session = MockSession {
            supports_exit: true,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ExitCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Exit));
    }

    #[tokio::test]
    async fn exit_sends_message_when_not_supported() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
        };
        let session = MockSession {
            supports_exit: false,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ExitCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Continue));
        assert!(!sink.sent.is_empty());
    }

    #[tokio::test]
    async fn clear_clears_history() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
        };
        let session = MockSession {
            supports_exit: false,
        };
        let out = {
            let mut agent = crate::NullAgent;
            let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
            ClearCommand.handle(&mut ctx, "").await.unwrap()
        };
        assert!(matches!(out, CommandOutput::Silent));
        assert!(messages.cleared);
    }

    #[tokio::test]
    async fn reset_clears_and_confirms() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
        };
        let session = MockSession {
            supports_exit: false,
        };
        let out = {
            let mut agent = crate::NullAgent;
            let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
            ResetCommand.handle(&mut ctx, "").await.unwrap()
        };
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("reset"));
        assert!(messages.cleared);
    }

    #[tokio::test]
    async fn clear_queue_drains_and_reports() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 3,
        };
        let session = MockSession {
            supports_exit: false,
        };
        let out = {
            let mut agent = crate::NullAgent;
            let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
            ClearQueueCommand.handle(&mut ctx, "").await.unwrap()
        };
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains('3'));
        assert_eq!(messages.queue, 0);
    }

    #[test]
    fn registry_finds_all_session_commands() {
        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(ExitCommand);
        reg.register(QuitCommand);
        reg.register(ClearCommand);
        reg.register(ResetCommand);
        reg.register(ClearQueueCommand);

        assert!(reg.find_handler("/exit").is_some());
        assert!(reg.find_handler("/quit").is_some());
        assert!(reg.find_handler("/clear").is_some());
        assert!(reg.find_handler("/reset").is_some());
        assert!(reg.find_handler("/clear-queue").is_some());
    }
}
