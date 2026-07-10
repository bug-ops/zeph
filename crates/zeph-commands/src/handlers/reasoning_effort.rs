// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/reasoning-effort` command handler — runtime reasoning-effort level control.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Show or set the active provider's runtime reasoning-effort level.
///
/// - `/reasoning-effort` — display the current level.
/// - `/reasoning-effort low|medium|high` — set the level.
///
/// Session-only: never persisted across restarts or `/provider` switches. Supported by
/// Claude (adaptive thinking), OpenAI/Compatible (`reasoning_effort`), and Gemini (thinking
/// level); other providers return an explicit "not supported" message.
pub struct ReasoningEffortCommand;

impl CommandHandler<CommandContext<'_>> for ReasoningEffortCommand {
    fn name(&self) -> &'static str {
        "/reasoning-effort"
    }

    fn description(&self) -> &'static str {
        "Show or set the active provider's runtime reasoning-effort level"
    }

    fn args_hint(&self) -> &'static str {
        "[low|medium|high]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Configuration
    }

    fn requires_auth(&self) -> bool {
        true
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.reasoning_effort.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_reasoning_effort(args).await;
                Ok(CommandOutput::message_or_silent(result))
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::test_helpers::{MockDebug, MockMessages, MockSession, make_ctx};
    use crate::sink::NullSink;
    use std::assert_matches;

    #[test]
    fn reasoning_effort_name_and_description() {
        assert_eq!(ReasoningEffortCommand.name(), "/reasoning-effort");
        assert!(!ReasoningEffortCommand.description().is_empty());
    }

    #[tokio::test]
    async fn reasoning_effort_returns_silent_when_agent_returns_empty() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ReasoningEffortCommand.handle(&mut ctx, "").await.unwrap();
        assert_matches!(out, CommandOutput::Silent);
    }
}
