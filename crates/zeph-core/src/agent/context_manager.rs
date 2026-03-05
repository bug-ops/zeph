// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::config::{CompressionConfig, RoutingConfig};
use crate::context::ContextBudget;

pub(crate) struct ContextManager {
    pub(super) budget: Option<ContextBudget>,
    pub(super) compaction_threshold: f32,
    pub(super) compaction_preserve_tail: usize,
    pub(super) prune_protect_tokens: usize,
    /// Compression configuration for proactive compression (#1161).
    pub(super) compression: CompressionConfig,
    /// Routing configuration for query-aware memory routing (#1162).
    pub(super) routing: RoutingConfig,
    /// Set to `true` when compaction or proactive compression fires in the current turn.
    /// Cleared at the start of each turn. Prevents double compaction per turn (CRIT-03).
    pub(super) compacted_this_turn: bool,
}

impl ContextManager {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            budget: None,
            compaction_threshold: 0.80,
            compaction_preserve_tail: 6,
            prune_protect_tokens: 40_000,
            compression: CompressionConfig::default(),
            routing: RoutingConfig::default(),
            compacted_this_turn: false,
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(super) fn should_compact(&self, cached_tokens: u64) -> bool {
        let Some(ref budget) = self.budget else {
            return false;
        };
        let used = usize::try_from(cached_tokens).unwrap_or(usize::MAX);
        let threshold = (budget.max_tokens() as f32 * self.compaction_threshold) as usize;
        let should = used > threshold;
        tracing::debug!(
            cached_tokens,
            threshold,
            should_compact = should,
            "context budget check"
        );
        should
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
        assert!((cm.compaction_threshold - 0.80).abs() < f32::EPSILON);
        assert_eq!(cm.compaction_preserve_tail, 6);
        assert_eq!(cm.prune_protect_tokens, 40_000);
        assert!(!cm.compacted_this_turn);
    }

    #[test]
    fn should_compact_no_budget() {
        let cm = ContextManager::new();
        assert!(!cm.should_compact(1_000_000));
    }

    #[test]
    fn should_compact_below_threshold() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100_000, 0.1));
        // threshold = 80_000; 1_000 < 80_000
        assert!(!cm.should_compact(1_000));
    }

    #[test]
    fn should_compact_above_threshold() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100, 0.1));
        cm.compaction_threshold = 0.01;
        // threshold = 1; 100 > 1
        assert!(cm.should_compact(100));
    }

    #[test]
    fn should_compact_at_zero_tokens() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100, 0.1));
        assert!(!cm.should_compact(0));
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
