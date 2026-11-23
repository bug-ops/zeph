// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Token usage accounting for ACP sessions.
//!
//! Gated behind the `unstable-session-usage` feature. Per-turn token deltas (from
//! `LoopbackEvent::Usage` events) are summed as [`TokenUsage`] and attached to the
//! `PromptResponse.usage` field, while [`SessionUsageAccumulator`] tracks lifetime totals used
//! to populate the session-close usage summary. Isolating this type keeps the feature's
//! surface out of the main agent dispatch logic in [`super`].

use zeph_common::money::Cents;
use zeph_llm::provider::TokenUsage;

/// Per-session token and cost totals used to populate the session-close usage summary.
///
/// Token fields accumulate per-call deltas. `last_cost_cents` and `last_context_window`
/// are overwritten on each update (already-cumulative / most-recent-valid values from
/// `LoopbackEvent::Usage`).
#[derive(Debug, Default, Clone)]
pub(crate) struct SessionUsageAccumulator {
    pub(crate) totals: TokenUsage,
    /// Cumulative cost in USD cents — overwrite on each update, do not sum.
    pub(crate) last_cost_cents: Cents,
    /// Most recent context window size in tokens — overwrite on each update.
    pub(crate) last_context_window: u64,
}

impl SessionUsageAccumulator {
    /// Record one LLM call's token deltas, latest cumulative cost, and context window size.
    pub(crate) fn record(&mut self, usage: TokenUsage, cost_cents: Cents, context_window: u64) {
        self.totals += usage;
        // cost_cents and context_window are snapshot values — overwrite, do not accumulate.
        self.last_cost_cents = cost_cents;
        self.last_context_window = context_window;
    }
}

#[cfg(test)]
mod tests {
    use super::{Cents, SessionUsageAccumulator, TokenUsage};
    use crate::agent::build_prompt_response;
    use agent_client_protocol as acp;

    #[test]
    fn session_accumulator_sums_tokens_and_overwrites_cost_and_context_window() {
        let mut acc = SessionUsageAccumulator::default();
        acc.record(
            TokenUsage {
                input: 100,
                output: 50,
                cache_read: 10,
                cache_write: 5,
            },
            Cents::new(1.5),
            128_000,
        );
        // cost and context_window must overwrite
        acc.record(
            TokenUsage {
                input: 200,
                output: 80,
                ..TokenUsage::default()
            },
            Cents::new(3.0),
            64_000,
        );
        assert_eq!(
            acc.totals,
            TokenUsage {
                input: 300,
                output: 130,
                cache_read: 10,
                cache_write: 5,
            }
        );
        assert_eq!(acc.last_cost_cents, Cents::new(3.0));
        assert_eq!(acc.last_context_window, 64_000);
    }

    #[test]
    fn session_accumulator_default_is_zero() {
        let acc = SessionUsageAccumulator::default();
        assert_eq!(acc.totals, TokenUsage::default());
        assert_eq!(acc.last_cost_cents, Cents::ZERO);
        assert_eq!(acc.last_context_window, 0);
    }

    #[test]
    fn build_prompt_response_attaches_usage() {
        let turn_usage = TokenUsage {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 0,
        };
        let resp = build_prompt_response(acp::v1::schema::StopReason::EndTurn, turn_usage);
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
        let turn_usage = TokenUsage::default();
        let resp = build_prompt_response(acp::v1::schema::StopReason::EndTurn, turn_usage);
        let u = resp
            .usage
            .expect("usage should be set even for zero tokens");
        assert_eq!(u.total_tokens, 0);
        assert_eq!(u.cached_read_tokens, None);
        assert_eq!(u.cached_write_tokens, None);
    }
}
