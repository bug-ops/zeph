// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::config::{CompressionConfig, RoutingConfig};
use crate::context::ContextBudget;

/// Indicates which compaction tier applies for the current context size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionTier {
    /// Context is within budget — no compaction needed.
    None,
    /// Soft tier: prune tool outputs + apply deferred summaries. No LLM call.
    Soft,
    /// Hard tier: full LLM-based summarization.
    Hard,
}

pub(crate) struct ContextManager {
    pub(super) budget: Option<ContextBudget>,
    /// Soft compaction threshold (default 0.70): prune tool outputs + apply deferred summaries.
    pub(super) soft_compaction_threshold: f32,
    /// Hard compaction threshold (default 0.90): full LLM-based summarization.
    pub(super) hard_compaction_threshold: f32,
    pub(super) compaction_preserve_tail: usize,
    pub(super) prune_protect_tokens: usize,
    /// Compression configuration for proactive compression (#1161).
    pub(super) compression: CompressionConfig,
    /// Routing configuration for query-aware memory routing (#1162).
    pub(super) routing: RoutingConfig,
    /// Set to `true` when compaction or proactive compression fires in the current turn.
    /// Cleared at the start of each turn. Prevents double LLM summarization per turn (CRIT-03).
    pub(super) compacted_this_turn: bool,
    /// Number of turns to skip compaction after a successful compaction (cooldown guard).
    /// Prevents compaction from re-triggering immediately when the summary itself is large.
    pub(super) compaction_cooldown_turns: u8,
    /// Remaining turns in the current cooldown. Counts down each turn; 0 means ready.
    pub(super) compaction_turns_since: u8,
    /// Set to `true` when compaction is counterproductive (summary >= freed tokens)
    /// or when context cannot be reduced below threshold. No further compaction is attempted.
    pub(super) compaction_exhausted: bool,
    /// Tracks whether the exhaustion warning message has been sent to the user.
    pub(super) exhaustion_warned: bool,
    /// Counts user-message turns since the last hard compaction event.
    /// `None` = no hard compaction has occurred yet in this session.
    /// `Some(n)` = n turns have elapsed since the last hard compaction.
    pub(super) turns_since_last_hard_compaction: Option<u64>,
}

impl ContextManager {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            budget: None,
            soft_compaction_threshold: 0.60,
            hard_compaction_threshold: 0.90,
            compaction_preserve_tail: 6,
            prune_protect_tokens: 40_000,
            compression: CompressionConfig::default(),
            routing: RoutingConfig::default(),
            compacted_this_turn: false,
            compaction_cooldown_turns: 2,
            compaction_turns_since: 0,
            compaction_exhausted: false,
            exhaustion_warned: false,
            turns_since_last_hard_compaction: None,
        }
    }

    /// Determine which compaction tier applies for the given token count.
    ///
    /// - `Hard` when `cached_tokens > budget * hard_compaction_threshold`
    /// - `Soft` when `cached_tokens > budget * soft_compaction_threshold`
    /// - `None` otherwise (or when no budget is set)
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(super) fn compaction_tier(&self, cached_tokens: u64) -> CompactionTier {
        let Some(ref budget) = self.budget else {
            return CompactionTier::None;
        };
        let used = usize::try_from(cached_tokens).unwrap_or(usize::MAX);
        let max = budget.max_tokens();
        let hard = (max as f32 * self.hard_compaction_threshold) as usize;
        if used > hard {
            tracing::debug!(
                cached_tokens,
                hard_threshold = hard,
                "context budget check: Hard tier"
            );
            return CompactionTier::Hard;
        }
        let soft = (max as f32 * self.soft_compaction_threshold) as usize;
        if used > soft {
            tracing::debug!(
                cached_tokens,
                soft_threshold = soft,
                "context budget check: Soft tier"
            );
            return CompactionTier::Soft;
        }
        tracing::debug!(
            cached_tokens,
            soft_threshold = soft,
            "context budget check: None"
        );
        CompactionTier::None
    }

    /// Build a memory router from the current routing configuration.
    ///
    /// The router is stateless and cheap to construct per turn.
    pub(super) fn build_router(&self) -> zeph_memory::HeuristicRouter {
        use crate::config::RoutingStrategy;
        match self.routing.strategy {
            RoutingStrategy::Heuristic => zeph_memory::HeuristicRouter,
        }
    }

    /// Check if proactive compression should fire for the current turn.
    ///
    /// Returns `Some((threshold_tokens, max_summary_tokens))` when proactive compression
    /// should be triggered, `None` otherwise.
    ///
    /// Will return `None` if compaction already happened this turn (CRIT-03 fix).
    pub(super) fn should_proactively_compress(
        &self,
        current_tokens: u64,
    ) -> Option<(usize, usize)> {
        use crate::config::CompressionStrategy;
        if self.compacted_this_turn {
            return None;
        }
        match &self.compression.strategy {
            CompressionStrategy::Proactive {
                threshold_tokens,
                max_summary_tokens,
                // On 32-bit targets (e.g. wasm32), u64 values above u32::MAX saturate to
                // usize::MAX, which always exceeds any threshold — intentionally conservative
                // (triggers compression rather than silently skipping it).
            } if usize::try_from(current_tokens).unwrap_or(usize::MAX) > *threshold_tokens => {
                Some((*threshold_tokens, *max_summary_tokens))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CompressionStrategy;

    #[test]
    fn new_defaults() {
        let cm = ContextManager::new();
        assert!(cm.budget.is_none());
        assert!((cm.soft_compaction_threshold - 0.60).abs() < f32::EPSILON);
        assert!((cm.hard_compaction_threshold - 0.90).abs() < f32::EPSILON);
        assert_eq!(cm.compaction_preserve_tail, 6);
        assert_eq!(cm.prune_protect_tokens, 40_000);
        assert!(!cm.compacted_this_turn);
    }

    #[test]
    fn compaction_tier_no_budget() {
        let cm = ContextManager::new();
        assert_eq!(cm.compaction_tier(1_000_000), CompactionTier::None);
    }

    #[test]
    fn compaction_tier_below_soft() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100_000, 0.1));
        // soft=60_000, hard=90_000; 50_000 < 60_000 → None
        assert_eq!(cm.compaction_tier(50_000), CompactionTier::None);
    }

    #[test]
    fn compaction_tier_between_soft_and_hard() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100_000, 0.1));
        // soft=60_000, hard=90_000; 75_000 > 60_000 && < 90_000 → Soft
        assert_eq!(cm.compaction_tier(75_000), CompactionTier::Soft);
    }

    #[test]
    fn compaction_tier_above_hard() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100_000, 0.1));
        // soft=60_000, hard=90_000; 95_000 > 90_000 → Hard
        assert_eq!(cm.compaction_tier(95_000), CompactionTier::Hard);
    }

    #[test]
    fn compaction_tier_at_zero_tokens() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100_000, 0.1));
        assert_eq!(cm.compaction_tier(0), CompactionTier::None);
    }

    #[test]
    fn compaction_tier_exact_soft_threshold() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100_000, 0.1));
        // soft=60_000; 60_000 is NOT > 60_000 → None (must exceed, not equal)
        assert_eq!(cm.compaction_tier(60_000), CompactionTier::None);
    }

    #[test]
    fn compaction_tier_exact_hard_threshold() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100_000, 0.1));
        // soft=60_000, hard=90_000; 90_000 is NOT > 90_000 → Soft (not Hard)
        assert_eq!(cm.compaction_tier(90_000), CompactionTier::Soft);
    }

    #[test]
    fn compaction_tier_custom_thresholds() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100, 0.1));
        cm.soft_compaction_threshold = 0.01;
        cm.hard_compaction_threshold = 0.50;
        // soft=1, hard=50; 100 > 50 → Hard
        assert_eq!(cm.compaction_tier(100), CompactionTier::Hard);
    }

    #[test]
    fn proactive_compress_reactive_strategy_returns_none() {
        let cm = ContextManager::new(); // Reactive by default
        assert!(cm.should_proactively_compress(100_000).is_none());
    }

    #[test]
    fn proactive_compress_below_threshold_returns_none() {
        let mut cm = ContextManager::new();
        cm.compression.strategy = CompressionStrategy::Proactive {
            threshold_tokens: 80_000,
            max_summary_tokens: 4_000,
        };
        assert!(cm.should_proactively_compress(50_000).is_none());
    }

    #[test]
    fn proactive_compress_above_threshold_returns_params() {
        let mut cm = ContextManager::new();
        cm.compression.strategy = CompressionStrategy::Proactive {
            threshold_tokens: 80_000,
            max_summary_tokens: 4_000,
        };
        let result = cm.should_proactively_compress(90_000);
        assert_eq!(result, Some((80_000, 4_000)));
    }

    #[test]
    fn proactive_compress_blocked_if_compacted_this_turn() {
        let mut cm = ContextManager::new();
        cm.compression.strategy = CompressionStrategy::Proactive {
            threshold_tokens: 80_000,
            max_summary_tokens: 4_000,
        };
        cm.compacted_this_turn = true;
        assert!(cm.should_proactively_compress(100_000).is_none());
    }
}
