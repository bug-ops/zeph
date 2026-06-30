// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/caveman` command handler — toggles ultra-compressed (telegraphic) output mode.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Toggle or query ultra-compressed (caveman) output mode.
///
/// - `/caveman` — toggle current state.
/// - `/caveman on` — activate.
/// - `/caveman off` — deactivate.
/// - `/caveman status` — report state without changing it.
pub struct CavemanCommand;

impl CommandHandler<CommandContext<'_>> for CavemanCommand {
    fn name(&self) -> &'static str {
        "/caveman"
    }

    fn description(&self) -> &'static str {
        "Toggle ultra-compressed (telegraphic) output mode"
    }

    fn args_hint(&self) -> &'static str {
        "[on|off|status]"
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
        let span = tracing::info_span!("commands.caveman.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_caveman(args).await;
                Ok(CommandOutput::Message(result))
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
    fn caveman_name_and_description() {
        assert_eq!(CavemanCommand.name(), "/caveman");
        assert!(!CavemanCommand.description().is_empty());
    }

    #[tokio::test]
    async fn caveman_returns_message_with_null_agent() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = CavemanCommand.handle(&mut ctx, "").await.unwrap();
        assert_matches!(out, CommandOutput::Message(_));
    }
}
