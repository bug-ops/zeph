// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session management command handlers: `/exit`, `/quit`, `/clear`, `/reset`, `/clear-queue`.

use std::future::Future;
use std::pin::Pin;

use crate::CommandHandler;
use crate::context::CommandContext;
use crate::{CommandError, CommandOutput, SlashCategory};

async fn handle_exit(ctx: &mut CommandContext<'_>) -> Result<CommandOutput, CommandError> {
    if ctx.session.supports_exit() {
        Ok(CommandOutput::Exit)
    } else {
        ctx.sink
            .send("/exit is not supported in this channel.")
            .await?;
        Ok(CommandOutput::Continue)
    }
}

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

    fn requires_auth(&self) -> bool {
        false
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.exit.handle");
        Box::pin(async move { handle_exit(ctx).await }.instrument(span))
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

    fn requires_auth(&self) -> bool {
        false
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.quit.handle");
        Box::pin(async move { handle_exit(ctx).await }.instrument(span))
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

    fn requires_auth(&self) -> bool {
        true
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.clear.handle");
        Box::pin(
            async move {
                ctx.messages.clear_history();
                Ok(CommandOutput::Silent)
            }
            .instrument(span),
        )
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

    fn requires_auth(&self) -> bool {
        true
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.reset.handle");
        Box::pin(
            async move {
                ctx.messages.clear_history();
                Ok(CommandOutput::Message(
                    "Conversation history reset.".to_owned(),
                ))
            }
            .instrument(span),
        )
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

    fn requires_auth(&self) -> bool {
        true
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        _args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.clear_queue.handle");
        Box::pin(
            async move {
                let n = ctx.messages.drain_queue();
                if let Err(e) = ctx.sink.send_queue_count(0).await {
                    tracing::debug!(
                        "clear_queue: send_queue_count notification failed (best-effort): {e}"
                    );
                }
                Ok(CommandOutput::Message(format!(
                    "Cleared {n} queued messages."
                )))
            }
            .instrument(span),
        )
    }
}

/// Show conversation history (spec-068 §13.6).
///
/// `/history` (no argument): the last `[session.resume] expand_default_lines` messages,
/// sliced before formatting (INV-SP-6). `/history N`: the last `N` messages. `/history all`:
/// pages through the full history in `expand_default_lines`-sized chunks — never
/// materializes the full history before formatting — with an explicit "may take a moment"
/// notice before the first page. `/history next`: continues from the pagination cursor left
/// by a prior `/history all` or `/history next`.
///
/// Output is sent via [`crate::sink::ChannelSink::send_transcript`], not the `handle`
/// return value, so channels with a structured display buffer (TUI) can backfill per-entry
/// instead of flattening to one string.
pub struct HistoryCommand;

enum HistoryArgs {
    Bounded(usize),
    All,
    Next,
}

fn parse_history_args(args: &str, default_lines: usize) -> HistoryArgs {
    match args.trim() {
        "" => HistoryArgs::Bounded(default_lines),
        "all" => HistoryArgs::All,
        "next" => HistoryArgs::Next,
        n => n
            .parse::<usize>()
            .map_or(HistoryArgs::Bounded(default_lines), HistoryArgs::Bounded),
    }
}

impl CommandHandler<CommandContext<'_>> for HistoryCommand {
    fn name(&self) -> &'static str {
        "/history"
    }

    fn description(&self) -> &'static str {
        "Show conversation history (N most recent messages, or 'all' to page through)"
    }

    fn args_hint(&self) -> &'static str {
        "[N|all|next]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Session
    }

    fn requires_auth(&self) -> bool {
        false
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.history.handle");
        Box::pin(
            async move {
                let default_lines = ctx.session.history_expand_default_lines().max(1);
                let total = ctx.messages.transcript_len();
                if total == 0 {
                    ctx.sink.send("No conversation history yet.").await?;
                    return Ok(CommandOutput::Silent);
                }

                match parse_history_args(args, default_lines) {
                    HistoryArgs::Bounded(n) => {
                        let n = n.min(total);
                        let start = total.saturating_sub(n);
                        let entries = ctx.messages.transcript_page(start, n);
                        ctx.messages.set_history_cursor(0);
                        ctx.sink.send_transcript(&entries).await?;
                    }
                    HistoryArgs::All => {
                        ctx.sink
                            .send(&format!(
                                "Showing full history ({total} messages) — this may take a moment."
                            ))
                            .await?;
                        let count = default_lines.min(total);
                        let entries = ctx.messages.transcript_page(0, count);
                        ctx.messages.set_history_cursor(count);
                        ctx.sink.send_transcript(&entries).await?;
                        if count < total {
                            let total_pages = total.div_ceil(default_lines);
                            ctx.sink
                                .send(&format!(
                                    "Page 1/{total_pages} — use /history next to continue."
                                ))
                                .await?;
                        }
                    }
                    HistoryArgs::Next => {
                        let cursor = ctx.messages.history_cursor();
                        if cursor == 0 || cursor >= total {
                            ctx.sink
                                .send(
                                    "No more history to page through. Use /history all to start over.",
                                )
                                .await?;
                            return Ok(CommandOutput::Silent);
                        }
                        let count = default_lines.min(total - cursor);
                        let entries = ctx.messages.transcript_page(cursor, count);
                        let new_cursor = cursor + count;
                        ctx.messages.set_history_cursor(new_cursor);
                        ctx.sink.send_transcript(&entries).await?;
                        if new_cursor < total {
                            // `page` is already 1-based (cursor=20, default_lines=20 -> page 2)
                            // — do not add another `+ 1` (M2: previously printed "Page 3/3" for
                            // what was actually page 2 of 3).
                            let page = cursor / default_lines + 1;
                            let total_pages = total.div_ceil(default_lines);
                            ctx.sink
                                .send(&format!(
                                    "Page {page}/{total_pages} — use /history next to continue."
                                ))
                                .await?;
                        }
                    }
                }
                Ok(CommandOutput::Silent)
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandRegistry;
    use crate::context::CommandContext;
    use crate::handlers::test_helpers::MockDebug;
    use crate::sink::ChannelSink;
    use crate::traits::messages::MessageAccess;
    use crate::traits::session::SessionAccess;
    use std::assert_matches;
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

    struct MockMessages {
        pub cleared: bool,
        pub queue: usize,
        pub transcript: Vec<crate::transcript::TranscriptEntry>,
        pub cursor: usize,
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

        fn transcript_len(&self) -> usize {
            self.transcript.len()
        }

        fn transcript_page(
            &self,
            start: usize,
            count: usize,
        ) -> Vec<crate::transcript::TranscriptEntry> {
            self.transcript
                .iter()
                .skip(start)
                .take(count)
                .cloned()
                .collect()
        }

        fn history_cursor(&self) -> usize {
            self.cursor
        }

        fn set_history_cursor(&mut self, pos: usize) {
            self.cursor = pos;
        }
    }

    struct MockSession {
        supports_exit: bool,
        expand_default_lines: usize,
    }

    impl SessionAccess for MockSession {
        fn supports_exit(&self) -> bool {
            self.supports_exit
        }

        fn history_expand_default_lines(&self) -> usize {
            self.expand_default_lines
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
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: true,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ExitCommand.handle(&mut ctx, "").await.unwrap();
        assert_matches!(out, CommandOutput::Exit);
    }

    #[tokio::test]
    async fn exit_sends_message_when_not_supported() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ExitCommand.handle(&mut ctx, "").await.unwrap();
        assert_matches!(out, CommandOutput::Continue);
        assert!(!sink.sent.is_empty());
    }

    #[tokio::test]
    async fn clear_clears_history() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let out = {
            let mut agent = crate::NullAgent;
            let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
            ClearCommand.handle(&mut ctx, "").await.unwrap()
        };
        assert_matches!(out, CommandOutput::Silent);
        assert!(messages.cleared);
    }

    #[tokio::test]
    async fn reset_clears_and_confirms() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
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
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
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
    fn exit_requires_auth_false() {
        assert!(!ExitCommand.requires_auth());
    }

    #[test]
    fn quit_requires_auth_false() {
        assert!(!QuitCommand.requires_auth());
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

    #[tokio::test]
    async fn clear_dispatch_allowed_when_trusted() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(ClearCommand);

        let result = reg.dispatch(&mut ctx, "/clear", true).await;
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn clear_dispatch_rejected_when_untrusted() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(ClearCommand);

        let result = reg.dispatch(&mut ctx, "/clear", false).await;
        let err = result.unwrap().unwrap_err();
        assert!(err.0.contains("trusted"));
    }

    #[tokio::test]
    async fn reset_dispatch_allowed_when_trusted() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(ResetCommand);

        let result = reg.dispatch(&mut ctx, "/reset", true).await;
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn reset_dispatch_rejected_when_untrusted() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(ResetCommand);

        let result = reg.dispatch(&mut ctx, "/reset", false).await;
        let err = result.unwrap().unwrap_err();
        assert!(err.0.contains("trusted"));
    }

    #[tokio::test]
    async fn clear_queue_dispatch_allowed_when_trusted() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(ClearQueueCommand);

        let result = reg.dispatch(&mut ctx, "/clear-queue", true).await;
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn clear_queue_dispatch_rejected_when_untrusted() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(ClearQueueCommand);

        let result = reg.dispatch(&mut ctx, "/clear-queue", false).await;
        let err = result.unwrap().unwrap_err();
        assert!(err.0.contains("trusted"));
    }

    // --- /history ---

    fn make_transcript(n: usize) -> Vec<crate::transcript::TranscriptEntry> {
        (0..n)
            .map(|i| crate::transcript::TranscriptEntry {
                role: crate::transcript::TranscriptRole::User,
                content: format!("message {i}"),
                tool_name: None,
            })
            .collect()
    }

    #[tokio::test]
    async fn history_no_history_yet() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: Vec::new(),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        let out = HistoryCommand.handle(&mut ctx, "").await.unwrap();
        assert_matches!(out, CommandOutput::Silent);
        assert!(sink.sent[0].contains("No conversation history"));
    }

    #[tokio::test]
    async fn history_default_bounds_to_expand_default_lines() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: make_transcript(500),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        HistoryCommand.handle(&mut ctx, "").await.unwrap();
        // Only the last 20 of 500 messages must ever reach the sink — never the full set
        // materialized then trimmed (INV-SP-6).
        assert_eq!(sink.sent.len(), 1);
        assert!(sink.sent[0].contains("message 480"));
        assert!(sink.sent[0].contains("message 499"));
        assert!(!sink.sent[0].contains("message 479"));
    }

    #[tokio::test]
    async fn history_numeric_argument_bounds_explicitly() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: make_transcript(10),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        HistoryCommand.handle(&mut ctx, "3").await.unwrap();
        assert_eq!(sink.sent.len(), 1);
        assert!(sink.sent[0].contains("message 7"));
        assert!(sink.sent[0].contains("message 9"));
        assert!(!sink.sent[0].contains("message 6"));
    }

    #[tokio::test]
    async fn history_all_shows_notice_then_paginates() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: make_transcript(50),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        HistoryCommand.handle(&mut ctx, "all").await.unwrap();
        assert!(sink.sent[0].contains("may take a moment"));
        assert!(sink.sent[1].contains("message 0"));
        assert!(!sink.sent[1].contains("message 20"));
        assert!(sink.sent[2].contains("Page 1/3"));
        assert_eq!(messages.history_cursor(), 20);
    }

    #[tokio::test]
    async fn history_next_continues_from_cursor() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: make_transcript(50),
            cursor: 20,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        HistoryCommand.handle(&mut ctx, "next").await.unwrap();
        assert!(sink.sent[0].contains("message 20"));
        assert!(sink.sent[0].contains("message 39"));
        assert!(!sink.sent[0].contains("message 40"));
        assert_eq!(messages.history_cursor(), 40);
        // M2 regression: cursor 20 -> 40 of 50 total is page 2 of 3, not "Page 3/3" (the
        // pre-fix off-by-one, which also contradicted its own "use /history next to continue"
        // text by claiming the last page still had more to show).
        assert_eq!(
            sink.sent[1], "Page 2/3 — use /history next to continue.",
            "page label must be 1-based without a stray extra +1"
        );
    }

    #[tokio::test]
    async fn history_next_without_prior_all_reports_nothing_to_page() {
        let mut sink = MockSink { sent: vec![] };
        let mut debug = MockDebug;
        let mut messages = MockMessages {
            cleared: false,
            queue: 0,
            transcript: make_transcript(50),
            cursor: 0,
        };
        let session = MockSession {
            supports_exit: false,
            expand_default_lines: 20,
        };
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);

        HistoryCommand.handle(&mut ctx, "next").await.unwrap();
        assert!(sink.sent[0].contains("No more history"));
    }

    #[test]
    fn history_requires_auth_false() {
        assert!(!HistoryCommand.requires_auth());
    }
}
