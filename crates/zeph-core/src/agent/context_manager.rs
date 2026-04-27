// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(test)]
pub(crate) use zeph_context::manager::CompactionTier;
pub(crate) use zeph_context::manager::{CompactionState, ContextManager};

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::CompressionStrategy;
    use zeph_context::budget::ContextBudget;

    #[test]
    fn new_defaults() {
        let cm = ContextManager::new();
        assert!(cm.budget.is_none());
        assert!((cm.soft_compaction_threshold - 0.60).abs() < f32::EPSILON);
        assert!((cm.hard_compaction_threshold - 0.90).abs() < f32::EPSILON);
        assert_eq!(cm.compaction_preserve_tail, 6);
        assert_eq!(cm.prune_protect_tokens, 40_000);
        assert_eq!(cm.compaction, CompactionState::Ready);
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
        cm.compaction = CompactionState::CompactedThisTurn { cooldown: 0 };
        assert!(cm.should_proactively_compress(100_000).is_none());
    }

    // ── CompactionState unit tests ──────────────────────────────────────────

    #[test]
    fn compaction_state_ready_is_not_compacted_this_turn() {
        assert!(!CompactionState::Ready.is_compacted_this_turn());
    }

    #[test]
    fn compaction_state_compacted_this_turn_flag() {
        assert!(CompactionState::CompactedThisTurn { cooldown: 2 }.is_compacted_this_turn());
        assert!(CompactionState::CompactedThisTurn { cooldown: 0 }.is_compacted_this_turn());
    }

    #[test]
    fn compaction_state_cooling_is_not_compacted_this_turn() {
        assert!(!CompactionState::Cooling { turns_remaining: 1 }.is_compacted_this_turn());
    }
}
