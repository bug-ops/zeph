// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::context::ContextBudget;

pub(crate) struct ContextManager {
    pub(super) budget: Option<ContextBudget>,
    pub(super) compaction_threshold: f32,
    pub(super) compaction_preserve_tail: usize,
    pub(super) prune_protect_tokens: usize,
}

impl ContextManager {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            budget: None,
            compaction_threshold: 0.80,
            compaction_preserve_tail: 6,
            prune_protect_tokens: 40_000,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults() {
        let cm = ContextManager::new();
        assert!(cm.budget.is_none());
        assert!((cm.compaction_threshold - 0.80).abs() < f32::EPSILON);
        assert_eq!(cm.compaction_preserve_tail, 6);
        assert_eq!(cm.prune_protect_tokens, 40_000);
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
}
