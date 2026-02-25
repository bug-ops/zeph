// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use zeph_memory::TokenCounter;

use crate::context::ContextBudget;

pub(crate) struct ContextManager {
    pub(super) budget: Option<ContextBudget>,
    pub(super) compaction_threshold: f32,
    pub(super) compaction_preserve_tail: usize,
    pub(super) prune_protect_tokens: usize,
    pub(crate) token_counter: Arc<TokenCounter>,
    pub(super) token_safety_margin: f32,
}

impl ContextManager {
    #[must_use]
    pub(crate) fn new(token_counter: Arc<TokenCounter>) -> Self {
        Self {
            budget: None,
            compaction_threshold: 0.80,
            compaction_preserve_tail: 6,
            prune_protect_tokens: 40_000,
            token_counter,
            token_safety_margin: 1.0,
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(super) fn should_compact(&self, messages: &[zeph_llm::provider::Message]) -> bool {
        let Some(ref budget) = self.budget else {
            return false;
        };
        let margin = self.token_safety_margin;
        let total_tokens: usize = messages
            .iter()
            .map(|m| {
                (self.token_counter.count_tokens(&m.content) as f64 * f64::from(margin)) as usize
            })
            .sum();
        let threshold = (budget.max_tokens() as f32 * self.compaction_threshold) as usize;
        let should = total_tokens > threshold;
        tracing::debug!(
            total_tokens,
            threshold,
            message_count = messages.len(),
            should_compact = should,
            "context budget check"
        );
        should
    }
}
