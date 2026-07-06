// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LSP status command handler: `/lsp`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Show LSP context injection status and session statistics.
pub struct LspCommand;

impl CommandHandler<CommandContext<'_>> for LspCommand {
    fn name(&self) -> &'static str {
        "/lsp"
    }

    fn description(&self) -> &'static str {
        "Show LSP context status"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Debugging
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("lsp-context")
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
        let span = tracing::info_span!("commands.lsp.handle");
        Box::pin(
            async move {
                let result = ctx.agent.lsp_status().await?;
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
    fn lsp_name_and_description() {
        assert_eq!(LspCommand.name(), "/lsp");
        assert!(!LspCommand.description().is_empty());
    }

    #[tokio::test]
    async fn lsp_returns_silent_when_agent_returns_empty() {
        // NullAgent returns empty string from lsp_status, so result is Silent.
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = LspCommand.handle(&mut ctx, "").await.unwrap();
        assert_matches!(out, CommandOutput::Silent);
    }
}
