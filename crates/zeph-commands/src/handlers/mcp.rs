// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MCP management handler: `/mcp`.
//!
//! Delegates to `AgentAccess::handle_mcp`, which in turn calls the
//! `Agent<C>` inherent methods in `zeph-core::agent::mcp`. Status messages
//! (`send_status`) are emitted as channel side effects inside the `Agent<C>`
//! implementation; only the final user-facing message is surfaced as the
//! command return value.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Manage MCP server connections.
///
/// Subcommands: `add`, `list`, `tools`, `remove`.
///
/// Delegates to `AgentAccess::handle_mcp`, which collects all output into
/// a `String` and returns it.  The registry sends the string to the channel
/// as a `Message` output.
pub struct McpCommand;

impl CommandHandler<CommandContext<'_>> for McpCommand {
    fn name(&self) -> &'static str {
        "/mcp"
    }

    fn description(&self) -> &'static str {
        "Manage MCP server connections"
    }

    fn args_hint(&self) -> &'static str {
        "add|list|tools|remove"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Integration
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let output = ctx.agent.handle_mcp(args).await?;
            Ok(CommandOutput::Message(output))
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
    fn mcp_name_and_description() {
        assert_eq!(McpCommand.name(), "/mcp");
        assert!(!McpCommand.description().is_empty());
    }

    #[tokio::test]
    async fn mcp_returns_message() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = McpCommand.handle(&mut ctx, "list").await.unwrap();
        assert!(matches!(out, CommandOutput::Message(_)));
    }
}
