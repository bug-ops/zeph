// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Planning handler: `/plan`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Planning handler for `/plan`.
///
/// Delegates to `AgentAccess::handle_plan` which dispatches to `dispatch_plan_command`.
/// The future is Send because `Agent<C>: Send` and no non-Send guards are held across
/// await boundaries.
pub struct PlanCommand;

impl CommandHandler<CommandContext<'_>> for PlanCommand {
    fn name(&self) -> &'static str {
        "/plan"
    }

    fn description(&self) -> &'static str {
        "Create or manage execution plans"
    }

    fn args_hint(&self) -> &'static str {
        "[goal|confirm|cancel|status|list|resume|retry]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Planning
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.plan.handle");
        Box::pin(
            async move {
                // Reconstruct the full command string so the plan parser can parse it.
                let input = if args.is_empty() {
                    "/plan".to_owned()
                } else {
                    format!("/plan {args}")
                };
                let result = ctx.agent.handle_plan(&input).await?;
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
    fn plan_name_and_description() {
        assert_eq!(PlanCommand.name(), "/plan");
        assert!(!PlanCommand.description().is_empty());
    }

    #[tokio::test]
    async fn plan_no_args_returns_silent_when_agent_returns_empty() {
        // NullAgent returns empty string from handle_plan, so result is Silent.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = PlanCommand.handle(&mut ctx, "").await.unwrap();
        assert!(matches!(out, CommandOutput::Silent));
    }

    #[tokio::test]
    async fn plan_with_args_returns_silent_when_agent_returns_empty() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = PlanCommand.handle(&mut ctx, "status").await.unwrap();
        assert!(matches!(out, CommandOutput::Silent));
    }
}
