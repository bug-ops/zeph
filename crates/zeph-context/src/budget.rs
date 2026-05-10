// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Token budget calculation for context assembly.
//!
//! [`ContextBudget`] tracks the maximum token count for a session and divides
//! available tokens across context slots (summaries, semantic recall, code context, etc.).
//! [`BudgetAllocation`] is the result of one budget split and is consumed by
//! [`crate::assembler::ContextAssembler`].

use zeph_common::memory::TokenCounting;

/// Per-slot token budget produced by [`ContextBudget::allocate`].
///
/// All fields are in tokens. Zero means the slot is disabled or budget-exhausted for this turn.
#[derive(Debug, Clone)]
pub struct BudgetAllocation {
    /// Tokens consumed by the current system prompt.
    pub system_prompt: usize,
    /// Tokens consumed by the current skills prompt.
    pub skills: usize,
    /// Tokens allocated for past-conversation summaries.
    pub summaries: usize,
    /// Tokens allocated for semantic (vector) recall results.
    pub semantic_recall: usize,
    /// Tokens allocated for cross-session memory recall.
    pub cross_session: usize,
    /// Tokens allocated for code-index RAG context.
    pub code_context: usize,
    /// Tokens reserved for graph facts. Always present; 0 when graph-memory is disabled.
    pub graph_facts: usize,
    /// Tokens allocated for recent conversation history trim.
    pub recent_history: usize,
    /// Tokens reserved for the model response (not filled by context sources).
    pub response_reserve: usize,
    /// Tokens pre-reserved for the session digest block. Always present; 0 when digest is
    /// disabled or no digest exists for the current conversation.
    pub session_digest: usize,
}

impl BudgetAllocation {
    /// Count of context source slots with non-zero token budgets.
    #[must_use]
    pub fn active_sources(&self) -> usize {
        [
            self.summaries,
            self.semantic_recall,
            self.cross_session,
            self.code_context,
            self.graph_facts,
        ]
        .iter()
        .filter(|&&t| t > 0)
        .count()
    }
}

/// Token budget for a single agent session.
///
/// Tracks the maximum token window and divides it across context slots.
/// Call [`ContextBudget::allocate`] or [`ContextBudget::allocate_with_opts`] to get a
/// [`BudgetAllocation`] that can be fed to [`crate::assembler::ContextAssembler`].
#[derive(Debug, Clone)]
pub struct ContextBudget {
    max_tokens: usize,
    reserve_ratio: f32,
    /// Whether graph-fact allocation is active. Toggles the 4% graph-facts slice.
    pub(crate) graph_enabled: bool,
}

impl ContextBudget {
    /// Create a new budget with `max_tokens` capacity and `reserve_ratio` fraction reserved
    /// for the model response.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_context::budget::ContextBudget;
    ///
    /// let budget = ContextBudget::new(128_000, 0.15);
    /// assert_eq!(budget.max_tokens(), 128_000);
    /// ```
    #[must_use]
    pub fn new(max_tokens: usize, reserve_ratio: f32) -> Self {
        Self {
            max_tokens,
            reserve_ratio,
            graph_enabled: false,
        }
    }

    /// Enable or disable graph fact allocation in the budget split.
    ///
    /// When enabled, 4% of available tokens are routed to the `graph_facts` slot, and the
    /// `summaries`/`semantic_recall` slices are each reduced by 1%.
    #[must_use]
    pub fn with_graph_enabled(mut self, enabled: bool) -> Self {
        self.graph_enabled = enabled;
        self
    }

    /// Maximum token capacity for this session.
    #[must_use]
    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    /// Allocate the budget across context slots for one turn.
    ///
    /// Equivalent to `allocate_with_opts(…, 0, false)`.
    ///
    /// # Examples
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_context::budget::ContextBudget;
    ///
    /// // Any type implementing `zeph_common::memory::TokenCounting` can be used.
    /// # struct Tc;
    /// # impl zeph_common::memory::TokenCounting for Tc {
    /// #     fn count_tokens(&self, t: &str) -> usize { t.split_whitespace().count() }
    /// #     fn count_tool_schema_tokens(&self, v: &serde_json::Value) -> usize { v.to_string().len() }
    /// # }
    /// let budget = ContextBudget::new(128_000, 0.15);
    /// let tc = Tc;
    /// let alloc = budget.allocate("system prompt", "skills prompt", &tc, false);
    /// assert!(alloc.recent_history > 0);
    /// ```
    #[must_use]
    pub fn allocate(
        &self,
        system_prompt: &str,
        skills_prompt: &str,
        tc: &dyn TokenCounting,
        graph_enabled: bool,
    ) -> BudgetAllocation {
        self.allocate_with_opts(system_prompt, skills_prompt, tc, graph_enabled, 0, false)
    }

    /// Allocate context budget with optional digest pre-reservation and `MemoryFirst` mode.
    ///
    /// `digest_tokens` — pre-counted tokens for the session digest block; deducted from
    /// available tokens BEFORE percentage splits so it does not silently crowd out other slots.
    ///
    /// `memory_first` — when `true`, sets `recent_history` to 0 and redistributes those
    /// tokens across `summaries`, `semantic_recall`, and `cross_session`.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn allocate_with_opts(
        &self,
        system_prompt: &str,
        skills_prompt: &str,
        tc: &dyn TokenCounting,
        graph_enabled: bool,
        digest_tokens: usize,
        memory_first: bool,
    ) -> BudgetAllocation {
        if self.max_tokens == 0 {
            return BudgetAllocation {
                system_prompt: 0,
                skills: 0,
                summaries: 0,
                semantic_recall: 0,
                cross_session: 0,
                code_context: 0,
                graph_facts: 0,
                recent_history: 0,
                response_reserve: 0,
                session_digest: 0,
            };
        }

        let response_reserve = (self.max_tokens as f32 * self.reserve_ratio) as usize;
        let mut available = self.max_tokens.saturating_sub(response_reserve);

        let system_prompt_tokens = tc.count_tokens(system_prompt);
        let skills_tokens = tc.count_tokens(skills_prompt);

        available = available.saturating_sub(system_prompt_tokens + skills_tokens);

        // Deduct digest tokens BEFORE percentage splits so the budget allocator accounts for them.
        let session_digest = digest_tokens.min(available);
        available = available.saturating_sub(session_digest);

        let (summaries, semantic_recall, cross_session, code_context, graph_facts, recent_history) =
            if memory_first {
                // MemoryFirst: no recent history, redistribute to memory slots.
                if graph_enabled {
                    (
                        (available as f32 * 0.22) as usize,
                        (available as f32 * 0.22) as usize,
                        (available as f32 * 0.12) as usize,
                        (available as f32 * 0.38) as usize,
                        (available as f32 * 0.06) as usize,
                        0,
                    )
                } else {
                    (
                        (available as f32 * 0.25) as usize,
                        (available as f32 * 0.25) as usize,
                        (available as f32 * 0.15) as usize,
                        (available as f32 * 0.35) as usize,
                        0,
                        0,
                    )
                }
            } else if graph_enabled {
                // When graph is enabled: take 4% for graph facts, reduce other slices by 1% each.
                (
                    (available as f32 * 0.07) as usize,
                    (available as f32 * 0.07) as usize,
                    (available as f32 * 0.03) as usize,
                    (available as f32 * 0.29) as usize,
                    (available as f32 * 0.04) as usize,
                    (available as f32 * 0.50) as usize,
                )
            } else {
                (
                    (available as f32 * 0.08) as usize,
                    (available as f32 * 0.08) as usize,
                    (available as f32 * 0.04) as usize,
                    (available as f32 * 0.30) as usize,
                    0,
                    (available as f32 * 0.50) as usize,
                )
            };

        BudgetAllocation {
            system_prompt: system_prompt_tokens,
            skills: skills_tokens,
            summaries,
            semantic_recall,
            cross_session,
            code_context,
            graph_facts,
            recent_history,
            response_reserve,
            session_digest,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

    use super::*;

    struct NaiveTc;
    impl TokenCounting for NaiveTc {
        fn count_tokens(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }
        fn count_tool_schema_tokens(&self, schema: &serde_json::Value) -> usize {
            schema.to_string().split_whitespace().count()
        }
    }

    #[test]
    fn context_budget_max_tokens_accessor() {
        let budget = ContextBudget::new(1000, 0.2);
        assert_eq!(budget.max_tokens(), 1000);
    }

    #[test]
    fn budget_allocation_basic() {
        let budget = ContextBudget::new(1000, 0.20);
        let tc = NaiveTc;
        let alloc = budget.allocate("system prompt", "skills prompt", &tc, false);
        assert_eq!(alloc.response_reserve, 200);
        assert!(alloc.system_prompt > 0);
        assert!(alloc.skills > 0);
        assert!(alloc.summaries > 0);
        assert!(alloc.semantic_recall > 0);
        assert!(alloc.recent_history > 0);
    }

    #[test]
    fn budget_allocation_zero_disables() {
        let tc = NaiveTc;
        let budget = ContextBudget::new(0, 0.20);
        let alloc = budget.allocate("test", "test", &tc, false);
        assert_eq!(alloc.system_prompt, 0);
        assert_eq!(alloc.skills, 0);
        assert_eq!(alloc.summaries, 0);
        assert_eq!(alloc.recent_history, 0);
    }

    #[test]
    fn budget_allocation_graph_disabled_no_graph_facts() {
        let tc = NaiveTc;
        let budget = ContextBudget::new(10_000, 0.20);
        let alloc = budget.allocate("", "", &tc, false);
        assert_eq!(alloc.graph_facts, 0);
        assert_eq!(alloc.summaries, (8_000_f32 * 0.08) as usize);
        assert_eq!(alloc.semantic_recall, (8_000_f32 * 0.08) as usize);
    }

    #[test]
    fn budget_allocation_graph_enabled_allocates_4_percent() {
        let tc = NaiveTc;
        let budget = ContextBudget::new(10_000, 0.20).with_graph_enabled(true);
        let alloc = budget.allocate("", "", &tc, true);
        assert!(alloc.graph_facts > 0);
        assert_eq!(alloc.summaries, (8_000_f32 * 0.07) as usize);
        assert_eq!(alloc.graph_facts, (8_000_f32 * 0.04) as usize);
    }

    #[test]
    fn budget_allocation_memory_first_zeroes_history() {
        let tc = NaiveTc;
        let budget = ContextBudget::new(10_000, 0.20);
        let alloc = budget.allocate_with_opts("", "", &tc, false, 0, true);
        assert_eq!(alloc.recent_history, 0);
        assert!(alloc.summaries > 0);
        assert!(alloc.semantic_recall > 0);
    }
}
