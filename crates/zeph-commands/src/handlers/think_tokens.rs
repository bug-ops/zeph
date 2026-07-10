// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/think-tokens` command handler — runtime Claude/Gemini thinking-token budget control.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Parse a `/think-tokens` argument into a token budget.
///
/// Accepts a bare integer, or an integer with a case-insensitive `k` (×1000) or `M`
/// (×`1_000_000`) suffix. One decimal place is allowed on the numeric part (e.g. `10.5k`),
/// rounded to the nearest integer. `0` and `off` (case-insensitive) both mean "disable" and
/// parse to `Ok(None)`. Negative numbers and malformed input return a descriptive `Err`.
///
/// # Examples
///
/// ```
/// use zeph_commands::handlers::think_tokens::parse_token_budget;
///
/// assert_eq!(parse_token_budget("8k"), Ok(Some(8_000)));
/// assert_eq!(parse_token_budget("10.5k"), Ok(Some(10_500)));
/// assert_eq!(parse_token_budget("1M"), Ok(Some(1_000_000)));
/// assert_eq!(parse_token_budget("off"), Ok(None));
/// assert_eq!(parse_token_budget("0"), Ok(None));
/// assert!(parse_token_budget("-1").is_err());
/// ```
///
/// # Errors
///
/// Returns `Err(String)` with a descriptive message when `arg` is empty, negative, or does
/// not parse as a number with an optional `k`/`M` suffix.
pub fn parse_token_budget(arg: &str) -> Result<Option<u32>, String> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return Err("empty token budget — expected a number (e.g. 8k, 1M, 0, off)".to_owned());
    }
    if trimmed.eq_ignore_ascii_case("off") {
        return Ok(None);
    }

    let (numeric, multiplier) = match trimmed.chars().last() {
        Some(c) if c.eq_ignore_ascii_case(&'k') => (&trimmed[..trimmed.len() - 1], 1_000.0),
        Some(c) if c.eq_ignore_ascii_case(&'m') => (&trimmed[..trimmed.len() - 1], 1_000_000.0),
        _ => (trimmed, 1.0),
    };

    if numeric.is_empty() {
        return Err(format!(
            "'{trimmed}' is missing a numeric value before the suffix"
        ));
    }

    let value: f64 = numeric
        .parse()
        .map_err(|_| format!("'{trimmed}' is not a valid token budget"))?;
    if value.is_sign_negative() {
        return Err(format!("token budget must not be negative: '{trimmed}'"));
    }

    let scaled = value * multiplier;
    if !scaled.is_finite() || scaled > f64::from(u32::MAX) {
        return Err(format!("'{trimmed}' is too large for a token budget"));
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rounded = scaled.round() as u32;
    if rounded == 0 {
        return Ok(None);
    }
    Ok(Some(rounded))
}

/// Show or set the active provider's runtime thinking-token budget.
///
/// - `/think-tokens` — display the current budget (or "off").
/// - `/think-tokens 8k` / `/think-tokens 8000` — set an explicit budget.
/// - `/think-tokens off` / `/think-tokens 0` — disable thinking.
///
/// Session-only: never persisted across restarts or `/provider` switches. Only Claude and
/// Gemini support a thinking-token budget; other providers return an explicit "not supported"
/// message.
pub struct ThinkTokensCommand;

impl CommandHandler<CommandContext<'_>> for ThinkTokensCommand {
    fn name(&self) -> &'static str {
        "/think-tokens"
    }

    fn description(&self) -> &'static str {
        "Show or set the active provider's runtime thinking-token budget"
    }

    fn args_hint(&self) -> &'static str {
        "[N|Nk|NM|off]"
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
        let span = tracing::info_span!("commands.think_tokens.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_think_tokens(args).await;
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
    fn think_tokens_name_and_description() {
        assert_eq!(ThinkTokensCommand.name(), "/think-tokens");
        assert!(!ThinkTokensCommand.description().is_empty());
    }

    #[tokio::test]
    async fn think_tokens_returns_silent_when_agent_returns_empty() {
        let mut sink = NullSink;
        let mut debug = MockDebug;
        let mut messages = MockMessages;
        let session = MockSession;
        let mut agent = crate::NullAgent;
        let mut ctx = make_ctx(&mut sink, &mut debug, &mut messages, &session, &mut agent);
        let out = ThinkTokensCommand.handle(&mut ctx, "").await.unwrap();
        assert_matches!(out, CommandOutput::Silent);
    }

    // ── parse_token_budget ───────────────────────────────────────────────

    #[test]
    fn parse_token_budget_empty_is_error() {
        assert!(parse_token_budget("").is_err());
        assert!(parse_token_budget("   ").is_err());
    }

    #[test]
    fn parse_token_budget_bare_k_is_error() {
        assert!(parse_token_budget("k").is_err());
    }

    #[test]
    fn parse_token_budget_negative_is_error() {
        assert!(parse_token_budget("-1").is_err());
    }

    #[test]
    fn parse_token_budget_malformed_compound_is_error() {
        assert!(parse_token_budget("1.2.3k").is_err());
    }

    #[test]
    fn parse_token_budget_off_disables() {
        assert_eq!(parse_token_budget("off"), Ok(None));
        assert_eq!(parse_token_budget("OFF"), Ok(None));
        assert_eq!(parse_token_budget("Off"), Ok(None));
    }

    #[test]
    fn parse_token_budget_zero_disables() {
        assert_eq!(parse_token_budget("0"), Ok(None));
    }

    #[test]
    fn parse_token_budget_k_suffix() {
        assert_eq!(parse_token_budget("8k"), Ok(Some(8_000)));
        assert_eq!(parse_token_budget("8K"), Ok(Some(8_000)));
    }

    #[test]
    fn parse_token_budget_decimal_k_suffix_rounds() {
        assert_eq!(parse_token_budget("10.5k"), Ok(Some(10_500)));
    }

    #[test]
    fn parse_token_budget_m_suffix() {
        assert_eq!(parse_token_budget("1M"), Ok(Some(1_000_000)));
        assert_eq!(parse_token_budget("1m"), Ok(Some(1_000_000)));
    }

    #[test]
    fn parse_token_budget_bare_integer() {
        assert_eq!(parse_token_budget("1024"), Ok(Some(1_024)));
    }

    #[test]
    fn parse_token_budget_overflow_is_error() {
        assert!(parse_token_budget("999999999999999M").is_err());
    }

    #[test]
    fn parse_token_budget_trims_whitespace() {
        assert_eq!(parse_token_budget("  8k  "), Ok(Some(8_000)));
    }
}
