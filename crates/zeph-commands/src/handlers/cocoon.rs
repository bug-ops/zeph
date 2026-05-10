// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/cocoon` slash command handler.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Inspect Cocoon sidecar status and model listing (`/cocoon status`, `/cocoon models`).
pub struct CocoonCommand;

impl CommandHandler<CommandContext<'_>> for CocoonCommand {
    fn name(&self) -> &'static str {
        "/cocoon"
    }

    fn description(&self) -> &'static str {
        "Inspect Cocoon sidecar (status, models)"
    }

    fn args_hint(&self) -> &'static str {
        "[status | models]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Integration
    }

    fn feature_gate(&self) -> Option<&'static str> {
        Some("cocoon")
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move { Ok(CommandOutput::Message(ctx.agent.handle_cocoon(args).await?)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_slash_cocoon() {
        assert_eq!(CocoonCommand.name(), "/cocoon");
    }

    #[test]
    fn category_is_integration() {
        assert_eq!(CocoonCommand.category(), SlashCategory::Integration);
    }

    #[test]
    fn feature_gate_is_cocoon() {
        assert_eq!(CocoonCommand.feature_gate(), Some("cocoon"));
    }

    #[test]
    fn description_and_args_hint_non_empty() {
        assert!(!CocoonCommand.description().is_empty());
        assert!(!CocoonCommand.args_hint().is_empty());
    }
}
