// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context lifecycle state machine for the Zeph agent.
//!
//! [`ContextManager`] tracks per-session compaction state and token budgets.
//! It decides when soft (pruning) or hard (LLM summarization) compaction should fire,
//! and builds the memory router used for query-aware store selection.
//!
//! [`CompactionState`] is the core state machine — see its doc comment for the
//! full transition map.

use std::sync::Arc;

use zeph_config::{CompressionConfig, StoreRoutingConfig};

use crate::budget::ContextBudget;

/// Lifecycle state of the compaction subsystem within a single session.
///
/// Replaces four independent boolean/u8 fields with an explicit state machine that makes
/// invalid states unrepresentable (e.g., warned-without-exhausted).
///
/// # Transition map
///
/// ```text
/// Ready
///   → CompactedThisTurn { cooldown } when hard compaction succeeds (pruning or LLM)
///   → CompactedThisTurn { cooldown: 0 } when focus truncation, eviction, or proactive
///     compression fires (these callers do not want post-compaction cooldown)
///   → Exhausted { warned: false } when compaction is counterproductive (too few messages,
///     zero net freed tokens, or still above hard threshold after LLM compaction)
///
/// CompactedThisTurn { cooldown }
///   → Cooling { turns_remaining: cooldown } when cooldown > 0  (via advance_turn)
///   → Ready                                 when cooldown == 0 (via advance_turn)
///
/// Cooling { turns_remaining }
///   → Cooling { turns_remaining - 1 } decremented inside maybe_compact each turn
///   → Ready                           when turns_remaining reaches 0
///   NOTE: Exhausted is NOT reachable from Cooling — all exhaustion-setting sites in
///   summarization.rs are guarded by an early-return when in_cooldown is true.
///
/// Exhausted { warned: false }
///   → Exhausted { warned: true } after the user warning is sent (one-shot)
///
/// Exhausted { warned: true }  (terminal — no further transitions)
/// ```
///
/// `turns_since_last_hard_compaction` is a **metric counter**, not part of this state machine,
/// and remains a separate field on `ContextManager`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionState {
    /// Normal state — compaction may fire if context exceeds thresholds.
    Ready,
    /// Hard compaction (or focus truncation / eviction / proactive compression) ran this turn.
    /// No further compaction until `advance_turn()` is called at the next turn boundary.
    /// `cooldown` carries the number of cooling turns to enforce after this turn ends.
    CompactedThisTurn {
        /// Cooling turns to enforce after this turn ends.
        cooldown: u8,
    },
    /// Cooling down after a recent hard compaction. Hard tier is skipped; soft is still allowed.
    /// Counter decrements inside `maybe_compact` each turn until it reaches 0.
    Cooling {
        /// Remaining cooling turns before returning to `Ready`.
        turns_remaining: u8,
    },
    /// Compaction cannot reduce context further. No more attempts will be made.
    /// `warned` tracks whether the one-shot user warning has been sent.
    Exhausted {
        /// Whether the user has already been notified of context exhaustion.
        warned: bool,
    },
}

impl CompactionState {
    /// Whether hard compaction (or a compaction-equivalent operation) already ran this turn.
    ///
    /// When `true`, `maybe_compact`, `maybe_proactive_compress`, and
    /// `maybe_soft_compact_mid_iteration` all skip execution (CRIT-03).
    #[must_use]
    pub fn is_compacted_this_turn(self) -> bool {
        matches!(self, Self::CompactedThisTurn { .. })
    }

    /// Whether compaction is permanently disabled for this session.
    #[must_use]
    pub fn is_exhausted(self) -> bool {
        matches!(self, Self::Exhausted { .. })
    }

    /// Remaining cooldown turns (0 when not in `Cooling` state).
    #[must_use]
    pub fn cooldown_remaining(self) -> u8 {
        match self {
            Self::Cooling { turns_remaining } => turns_remaining,
            _ => 0,
        }
    }

    /// Transition to the next-turn state at the start of each user turn.
    ///
    /// **Must be called exactly once per turn, before any compaction, eviction, or
    /// focus truncation can run.** This guarantees that `is_compacted_this_turn()`
    /// returns `false` when the sidequest check executes — preserving the invariant
    /// that the sidequest only sees same-turn compaction set by eviction which runs
    /// *after* this call.
    ///
    /// Transitions:
    /// - `CompactedThisTurn { cooldown: 0 }` → `Ready`
    /// - `CompactedThisTurn { cooldown: n }` → `Cooling { turns_remaining: n }`
    /// - All other states are returned unchanged.
    #[must_use]
    pub fn advance_turn(self) -> Self {
        match self {
            Self::CompactedThisTurn { cooldown } if cooldown > 0 => Self::Cooling {
                turns_remaining: cooldown,
            },
            Self::CompactedThisTurn { .. } => Self::Ready,
            other => other,
        }
    }
}

/// Indicates which compaction tier applies for the current context size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTier {
    /// Context is within budget — no compaction needed.
    None,
    /// Soft tier: prune tool outputs + apply deferred summaries. No LLM call.
    Soft,
    /// Hard tier: full LLM-based summarization.
    Hard,
}

/// Per-session context lifecycle manager.
///
/// Holds the token budget, compaction lifecycle state, and routing configuration.
/// Callers in `zeph-core` drive the state machine via `advance_turn`, `compaction_tier`,
/// and related accessors; the assembler reads the budget via `build_router` and field access.
pub struct ContextManager {
    /// Token budget for this session. `None` until configured via `apply_budget_config`.
    pub budget: Option<ContextBudget>,
    /// Soft compaction threshold (default 0.70): prune tool outputs + apply deferred summaries.
    pub soft_compaction_threshold: f32,
    /// Hard compaction threshold (default 0.90): full LLM-based summarization.
    pub hard_compaction_threshold: f32,
    /// Number of recent messages preserved during hard compaction.
    pub compaction_preserve_tail: usize,
    /// Token count protected from pruning during soft compaction.
    pub prune_protect_tokens: usize,
    /// Compression configuration for proactive compression.
    pub compression: CompressionConfig,
    /// Routing configuration for query-aware memory routing.
    pub routing: StoreRoutingConfig,
    /// Resolved provider for LLM/hybrid routing. `None` when strategy is `Heuristic`
    /// or when the named provider could not be resolved from the pool.
    pub store_routing_provider: Option<Arc<zeph_llm::any::AnyProvider>>,
    /// Compaction lifecycle state. Replaces four independent boolean/u8 fields to make
    /// invalid states unrepresentable. See [`CompactionState`] for the full transition map.
    pub compaction: CompactionState,
    /// Number of cooling turns to enforce after a successful hard compaction.
    pub compaction_cooldown_turns: u8,
    /// Counts user-message turns since the last hard compaction event.
    /// `None` = no hard compaction has occurred yet in this session.
    /// `Some(n)` = n turns have elapsed since the last hard compaction.
    pub turns_since_last_hard_compaction: Option<u64>,
}

impl ContextManager {
    /// Create a new `ContextManager` with default thresholds and no budget.
    #[must_use]
    pub fn new() -> Self {
        Self {
            budget: None,
            soft_compaction_threshold: 0.60,
            hard_compaction_threshold: 0.90,
            compaction_preserve_tail: 6,
            prune_protect_tokens: 40_000,
            compression: CompressionConfig::default(),
            routing: StoreRoutingConfig::default(),
            store_routing_provider: None,
            compaction: CompactionState::Ready,
            compaction_cooldown_turns: 2,
            turns_since_last_hard_compaction: None,
        }
    }

    /// Apply budget and compaction thresholds from config.
    ///
    /// Must be called once after config is resolved. Safe to call again when config reloads.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_budget_config(
        &mut self,
        budget_tokens: usize,
        reserve_ratio: f32,
        hard_compaction_threshold: f32,
        compaction_preserve_tail: usize,
        prune_protect_tokens: usize,
        soft_compaction_threshold: f32,
        compaction_cooldown_turns: u8,
    ) {
        if budget_tokens == 0 {
            tracing::warn!("context budget is 0 — agent will have no token tracking");
        }
        if budget_tokens > 0 {
            self.budget = Some(ContextBudget::new(budget_tokens, reserve_ratio));
        }
        self.hard_compaction_threshold = hard_compaction_threshold;
        self.compaction_preserve_tail = compaction_preserve_tail;
        self.prune_protect_tokens = prune_protect_tokens;
        self.soft_compaction_threshold = soft_compaction_threshold;
        self.compaction_cooldown_turns = compaction_cooldown_turns;
    }

    /// Reset compaction state for a new conversation.
    ///
    /// Clears cooldown, exhaustion, and turn counters so the new conversation starts
    /// with a clean compaction slate.
    pub fn reset_compaction(&mut self) {
        self.compaction = CompactionState::Ready;
        self.turns_since_last_hard_compaction = None;
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
    pub fn compaction_tier(&self, cached_tokens: u64) -> CompactionTier {
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
    /// Returns a `Box<dyn AsyncMemoryRouter>` so callers can use `route_async()` for LLM-based
    /// classification. `HeuristicRouter` implements `AsyncMemoryRouter` via a blanket impl that
    /// delegates to the sync `route_with_confidence`.
    pub fn build_router(&self) -> Box<dyn zeph_memory::AsyncMemoryRouter + Send + Sync> {
        use zeph_config::StoreRoutingStrategy;
        if !self.routing.enabled {
            return Box::new(zeph_memory::HeuristicRouter);
        }
        let fallback = zeph_memory::router::parse_route_str(
            &self.routing.fallback_route,
            zeph_memory::MemoryRoute::Hybrid,
        );
        match self.routing.strategy {
            StoreRoutingStrategy::Heuristic => Box::new(zeph_memory::HeuristicRouter),
            StoreRoutingStrategy::Llm => {
                let Some(provider) = self.store_routing_provider.clone() else {
                    tracing::warn!(
                        "store_routing: strategy=llm but no provider resolved; \
                         falling back to heuristic"
                    );
                    return Box::new(zeph_memory::HeuristicRouter);
                };
                Box::new(zeph_memory::LlmRouter::new(provider, fallback))
            }
            StoreRoutingStrategy::Hybrid => {
                let Some(provider) = self.store_routing_provider.clone() else {
                    tracing::warn!(
                        "store_routing: strategy=hybrid but no provider resolved; \
                         falling back to heuristic"
                    );
                    return Box::new(zeph_memory::HeuristicRouter);
                };
                Box::new(zeph_memory::HybridRouter::new(
                    provider,
                    fallback,
                    self.routing.confidence_threshold,
                ))
            }
        }
    }

    /// Check if proactive compression should fire for the current turn.
    ///
    /// Returns `Some((threshold_tokens, max_summary_tokens))` when proactive compression
    /// should be triggered, `None` otherwise.
    ///
    /// Will return `None` if compaction already happened this turn (CRIT-03 fix).
    #[must_use]
    pub fn should_proactively_compress(&self, current_tokens: u64) -> Option<(usize, usize)> {
        use zeph_config::CompressionStrategy;
        if self.compaction.is_compacted_this_turn() {
            return None;
        }
        match &self.compression.strategy {
            CompressionStrategy::Proactive {
                threshold_tokens,
                max_summary_tokens,
            } if usize::try_from(current_tokens).unwrap_or(usize::MAX) > *threshold_tokens => {
                Some((*threshold_tokens, *max_summary_tokens))
            }
            _ => None,
        }
    }
}

impl Default for ContextManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::CompressionStrategy;

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
        assert_eq!(cm.compaction_tier(50_000), CompactionTier::None);
    }

    #[test]
    fn compaction_tier_between_soft_and_hard() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100_000, 0.1));
        assert_eq!(cm.compaction_tier(75_000), CompactionTier::Soft);
    }

    #[test]
    fn compaction_tier_above_hard() {
        let mut cm = ContextManager::new();
        cm.budget = Some(ContextBudget::new(100_000, 0.1));
        assert_eq!(cm.compaction_tier(95_000), CompactionTier::Hard);
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

    #[test]
    fn advance_turn_compacted_with_cooldown_enters_cooling() {
        let state = CompactionState::CompactedThisTurn { cooldown: 3 };
        assert_eq!(
            state.advance_turn(),
            CompactionState::Cooling { turns_remaining: 3 }
        );
    }

    #[test]
    fn advance_turn_compacted_zero_cooldown_returns_ready() {
        let state = CompactionState::CompactedThisTurn { cooldown: 0 };
        assert_eq!(state.advance_turn(), CompactionState::Ready);
    }
}
