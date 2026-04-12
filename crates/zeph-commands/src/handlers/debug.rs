// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Debug command handlers: `/log`, `/debug-dump`, `/dump-format`.

use std::future::Future;
use std::pin::Pin;

use crate::CommandHandler;
use crate::context::CommandContext;
use crate::{CommandError, CommandOutput, SlashCategory};

/// Show log file path and recent log entries.
pub struct LogCommand;

impl CommandHandler<CommandContext<'_>> for LogCommand {
    fn name(&self) -> &'static str {
        "/log"
    }

    fn description(&self) -> &'static str {
        "Toggle verbose log output"
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
            let mut out = ctx.debug.log_status();
            if let Some(tail) = ctx.debug.read_log_tail(20).await {
                out.push('\n');
                out.push_str("Recent entries:\n");
                out.push_str(&ctx.debug.scrub(&tail));
            }
            Ok(CommandOutput::Message(out.trim_end().to_owned()))
        })
    }
}

/// Enable or show the status of debug dump output.
///
/// With no arguments, reports whether debug dump is active and where.
/// With a path argument, enables debug dump to that directory.
pub struct DebugDumpCommand;

impl CommandHandler<CommandContext<'_>> for DebugDumpCommand {
    fn name(&self) -> &'static str {
        "/debug-dump"
    }

    fn description(&self) -> &'static str {
        "Enable or toggle debug dump output"
    }

    fn args_hint(&self) -> &'static str {
        "[path]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            if args.is_empty() {
                let msg = match ctx.debug.dump_status() {
                    Some(path) => format!("Debug dump active: {path}"),
                    None => "Debug dump is inactive. Use `/debug-dump <path>` to enable, \
                         or start with `--debug-dump [dir]`."
                        .to_owned(),
                };
                return Ok(CommandOutput::Message(msg));
            }

            match ctx.debug.enable_dump(args) {
                Ok(path) => Ok(CommandOutput::Message(format!(
                    "Debug dump enabled: {path}"
                ))),
                Err(e) => Ok(CommandOutput::Message(format!(
                    "Failed to enable debug dump: {e}"
                ))),
            }
        })
    }
}

/// Switch debug dump format at runtime.
pub struct DumpFormatCommand;

impl CommandHandler<CommandContext<'_>> for DumpFormatCommand {
    fn name(&self) -> &'static str {
        "/dump-format"
    }

    fn description(&self) -> &'static str {
        "Switch debug dump format at runtime"
    }

    fn args_hint(&self) -> &'static str {
        "<json|raw|trace>"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            if args.is_empty() {
                return Ok(CommandOutput::Message(format!(
                    "Current dump format: {}. Use `/dump-format json|raw|trace` to change.",
                    ctx.debug.dump_format_name()
                )));
            }

            match ctx.debug.set_dump_format(args) {
                Ok(()) => Ok(CommandOutput::Message(format!(
                    "Debug dump format set to: {args}"
                ))),
                Err(e) => Ok(CommandOutput::Message(e.to_string())),
            }
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

    struct MockSession;

    impl SessionAccess for MockSession {
        fn supports_exit(&self) -> bool {
            false
        }
    }

    struct MockSink;

    impl ChannelSink for MockSink {
        fn send<'a>(
            &'a mut self,
            _msg: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), CommandError>> + Send + 'a>> {
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

    struct MockDebug {
        dump_active: bool,
        format: String,
        enable_result: Result<String, String>,
        set_format_result: Result<(), String>,
    }

    impl MockDebug {
        fn ok() -> Self {
            Self {
                dump_active: false,
                format: "raw".to_owned(),
                enable_result: Ok("/tmp/dump".to_owned()),
                set_format_result: Ok(()),
            }
        }
    }

    impl DebugAccess for MockDebug {
        fn log_status(&self) -> String {
            "Log file:  <disabled>\n".to_owned()
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
            if self.dump_active {
                Some("/tmp/dump".to_owned())
            } else {
                None
            }
        }

        fn dump_format_name(&self) -> String {
            self.format.clone()
        }

        fn enable_dump(&mut self, _dir: &str) -> Result<String, CommandError> {
            self.enable_result.clone().map_err(CommandError::new)
        }

        fn set_dump_format(&mut self, _name: &str) -> Result<(), CommandError> {
            self.set_format_result.clone().map_err(CommandError::new)
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

    #[tokio::test]
    async fn log_command_formats_status() {
        let mut sink = MockSink;
        let mut debug = MockDebug::ok();
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = LogCommand.handle(&mut ctx, "").await.unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("<disabled>"));
    }

    #[tokio::test]
    async fn debug_dump_no_args_reports_inactive() {
        let mut sink = MockSink;
        let mut debug = MockDebug::ok();
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = DebugDumpCommand.handle(&mut ctx, "").await.unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("inactive"));
    }

    #[tokio::test]
    async fn debug_dump_with_path_enables_dump() {
        let mut sink = MockSink;
        let mut debug = MockDebug::ok();
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = DebugDumpCommand
            .handle(&mut ctx, "/tmp/dump")
            .await
            .unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("enabled"));
    }

    #[tokio::test]
    async fn dump_format_no_args_shows_current() {
        let mut sink = MockSink;
        let mut debug = MockDebug::ok();
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = DumpFormatCommand.handle(&mut ctx, "").await.unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("raw"));
    }

    #[tokio::test]
    async fn dump_format_with_arg_switches_format() {
        let mut sink = MockSink;
        let mut debug = MockDebug::ok();
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = DumpFormatCommand.handle(&mut ctx, "json").await.unwrap();
        let CommandOutput::Message(msg) = out else {
            panic!("expected Message")
        };
        assert!(msg.contains("json"));
    }

    #[test]
    fn registry_finds_all_debug_commands() {
        let mut reg: CommandRegistry<CommandContext<'_>> = CommandRegistry::new();
        reg.register(LogCommand);
        reg.register(DebugDumpCommand);
        reg.register(DumpFormatCommand);

        assert!(reg.find_handler("/log").is_some());
        assert!(reg.find_handler("/debug-dump").is_some());
        assert!(reg.find_handler("/dump-format").is_some());
    }
}
