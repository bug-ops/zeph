// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Token usage accounting for ACP sessions.
//!
//! Gated behind the `unstable-session-usage` feature. [`TurnUsage`] sums the token deltas of
//! a single prompt turn (from `LoopbackEvent::Usage` events) so they can be attached to the
//! `PromptResponse.usage` field, while [`SessionUsageAccumulator`] tracks lifetime totals used
//! to populate the session-close usage summary. Isolating these types keeps the feature's
//! surface out of the main agent dispatch logic in [`super`].

/// Per-turn token totals accumulated inside `drain_agent_events` for `PromptResponse.usage`.
///
/// Holds the sum of all `LoopbackEvent::Usage` events received within a single prompt turn.
#[allow(clippy::struct_field_names)] // all fields intentionally share `_tokens` postfix for clarity
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct TurnUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
}

/// Per-session token and cost totals used to populate the session-close usage summary.
///
/// Token fields accumulate per-call deltas. `last_cost_cents` and `last_context_window`
/// are overwritten on each update (already-cumulative / most-recent-valid values from
/// `LoopbackEvent::Usage`).
#[derive(Debug, Default, Clone)]
pub(crate) struct SessionUsageAccumulator {
    pub(crate) total_input_tokens: u64,
    pub(crate) total_output_tokens: u64,
    pub(crate) total_cache_read_tokens: u64,
    pub(crate) total_cache_write_tokens: u64,
    /// Cumulative cost in USD cents — overwrite on each update, do not sum.
    pub(crate) last_cost_cents: f64,
    /// Most recent context window size in tokens — overwrite on each update.
    pub(crate) last_context_window: u64,
}

impl SessionUsageAccumulator {
    /// Record one LLM call's token deltas, latest cumulative cost, and context window size.
    pub(crate) fn record(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        cost_cents: f64,
        context_window: u64,
    ) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(input_tokens);
        self.total_output_tokens = self.total_output_tokens.saturating_add(output_tokens);
        self.total_cache_read_tokens = self
            .total_cache_read_tokens
            .saturating_add(cache_read_tokens);
        self.total_cache_write_tokens = self
            .total_cache_write_tokens
            .saturating_add(cache_write_tokens);
        // cost_cents and context_window are snapshot values — overwrite, do not accumulate.
        self.last_cost_cents = cost_cents;
        self.last_context_window = context_window;
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionUsageAccumulator, TurnUsage};
    use crate::agent::build_prompt_response;
    use agent_client_protocol as acp;

    #[test]
    fn session_accumulator_sums_tokens_and_overwrites_cost_and_context_window() {
        let mut acc = SessionUsageAccumulator::default();
        acc.record(100, 50, 10, 5, 1.5, 128_000);
        acc.record(200, 80, 0, 0, 3.0, 64_000); // cost and context_window must overwrite
        assert_eq!(acc.total_input_tokens, 300);
        assert_eq!(acc.total_output_tokens, 130);
        assert_eq!(acc.total_cache_read_tokens, 10);
        assert_eq!(acc.total_cache_write_tokens, 5);
        assert!((acc.last_cost_cents - 3.0).abs() < f64::EPSILON);
        assert_eq!(acc.last_context_window, 64_000);
    }

    #[test]
    fn session_accumulator_default_is_zero() {
        let acc = SessionUsageAccumulator::default();
        assert_eq!(acc.total_input_tokens, 0);
        assert!(acc.last_cost_cents.abs() < f64::EPSILON);
        assert_eq!(acc.last_context_window, 0);
    }

    #[test]
    fn build_prompt_response_attaches_usage() {
        let turn_usage = TurnUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 0,
        };
        let resp = build_prompt_response(acp::schema::StopReason::EndTurn, turn_usage);
        let u = resp.usage.expect("usage should be set");
        assert_eq!(u.total_tokens, 150);
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
        // cache_read_tokens > 0 → field should be set
        assert_eq!(u.cached_read_tokens, Some(10));
        // cache_write_tokens == 0 → field should be None
        assert_eq!(u.cached_write_tokens, None);
        // thought_tokens not tracked for MVP
        assert_eq!(u.thought_tokens, None);
    }

    #[test]
    fn build_prompt_response_zero_usage_still_attaches() {
        let turn_usage = TurnUsage::default();
        let resp = build_prompt_response(acp::schema::StopReason::EndTurn, turn_usage);
        let u = resp
            .usage
            .expect("usage should be set even for zero tokens");
        assert_eq!(u.total_tokens, 0);
        assert_eq!(u.cached_read_tokens, None);
        assert_eq!(u.cached_write_tokens, None);
    }
}
