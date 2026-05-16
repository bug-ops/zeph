// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Policy command handler: `/policy`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Inspect policy status or dry-run evaluation.
///
/// Subcommands: `status` (default), `check <tool> [args_json]`.
pub struct PolicyCommand;

impl CommandHandler<CommandContext<'_>> for PolicyCommand {
    fn name(&self) -> &'static str {
        "/policy"
    }

    fn description(&self) -> &'static str {
        "Inspect policy status or dry-run evaluation"
    }

    fn args_hint(&self) -> &'static str {
        "[status|check <tool> [args_json]]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("policy-enforcer")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.policy.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_policy(args).await?;
                if result.is_empty() {
                    Ok(CommandOutput::Silent)
                } else {
                    Ok(CommandOutput::Message(result))
                }
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
    fn policy_name_and_description() {
        assert_eq!(PolicyCommand.name(), "/policy");
        assert!(!PolicyCommand.description().is_empty());
    }

    #[tokio::test]
    async fn policy_returns_silent_when_agent_returns_empty() {
        // NullAgent returns empty string, so result is Silent.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = PolicyCommand.handle(&mut ctx, "status").await.unwrap();
        assert!(matches!(out, CommandOutput::Silent));
    }
}
