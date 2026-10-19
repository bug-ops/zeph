// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context layout pinning, frozen session prompt and turn directives (spec 088).
//!
//! The `append` layout keeps every request a byte extension of the previous one: the system
//! prompt is rendered once per conversation boundary, and everything that used to be rewritten
//! per turn (active skill bodies, matched MCP tools, environment, notices, budget) travels as a
//! `Directives` [`TurnContextBlock`] on the turn input instead.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use zeph_agent_context::SoftTierGate;
use zeph_common::SkillTrustLevel;
use zeph_config::{ContextLayout, ContextStrategy};
use zeph_context::manager::CompactionTier;
use zeph_llm::provider::{
    ConversationKey, LlmProvider as _, PrefixCache, RequestPolicy, RetrievalKey, ThinkingScope,
};

use super::directives::{AppendTurnInputs, PromptInputs, SessionPromptKey};

/// Turn inputs plus the previous and current prompt keys when the prompt was not re-rendered.
pub(super) type TurnInputs = (
    AppendTurnInputs,
    Option<(SessionPromptKey, SessionPromptKey)>,
);
use super::scrub::ScrubProgress;

use crate::agent::Agent;
use crate::channel::Channel;

/// Layout a conversation is pinned to; never changes until the next pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PinnedLayout {
    /// Legacy layout: per-turn system rebuild and head injection.
    Replace,
    /// Append-only layout; `key` identifies the conversation for the provider's prefix guard.
    Append {
        /// Conversation identity stamped on every request.
        key: ConversationKey,
    },
}

impl PinnedLayout {
    /// Whether the conversation uses the append-only layout.
    pub(crate) fn is_append(self) -> bool {
        matches!(self, Self::Append { .. })
    }

    /// Per-request policy for the provider; `None` keeps the provider's legacy behavior.
    pub(crate) fn request_policy(self) -> Option<RequestPolicy> {
        match self {
            Self::Replace => None,
            Self::Append { key } => Some(RequestPolicy {
                thinking: ThinkingScope::Verbatim,
                conversation: key,
            }),
        }
    }

    /// Short label for status output and tracing.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Append { .. } => "append",
        }
    }
}

/// Whether the configured layout resolves to `append`.
///
/// `MemoryFirst` and `Adaptive` strategies drain history every turn, so they always resolve to
/// `replace` and a conversation never mixes the artifacts of both layouts.
pub(crate) fn resolves_to_append(
    configured: ContextLayout,
    strategy: ContextStrategy,
    cache: PrefixCache,
) -> bool {
    if matches!(
        strategy,
        ContextStrategy::MemoryFirst | ContextStrategy::Adaptive
    ) {
        return false;
    }
    match configured {
        ContextLayout::Append => true,
        ContextLayout::Auto => cache.is_prefix(),
        _ => false,
    }
}

/// Share of growth over the last sweep that re-arms the sweeps in the soft compaction band.
const SWEEP_REARM_PERCENT: u64 = 110;

/// Whether the provider's prefix cache is still alive, judged from the idle time since the last
/// assistant reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheWarmth {
    /// Idle for less than the cache TTL.
    Warm,
    /// Idle for at least the cache TTL; the next request pays for a full prefix write anyway.
    Expired,
    /// The provider reports no TTL, or no assistant reply exists yet.
    Unknown,
}

impl CacheWarmth {
    /// Classify `idle` against `ttl`; `Unknown` unless both are known.
    pub(crate) fn classify(ttl: Option<Duration>, idle: Option<Duration>) -> Self {
        match (ttl, idle) {
            (Some(ttl), Some(idle)) if idle >= ttl => Self::Expired,
            (Some(_), Some(_)) => Self::Warm,
            _ => Self::Unknown,
        }
    }
}

/// Why this turn's sweeps may rewrite old messages, decided once at the start of the turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SweepGate {
    /// No sweep may rewrite history.
    #[default]
    Closed,
    /// The provider cache expired before the turn: only the sweeps that run before the turn's
    /// first request or at the turn boundary may rewrite history, because the turn's own
    /// requests re-warm the cache and a later rewrite would bust it again.
    CacheExpired,
    /// Token pressure (soft-band growth or the hard tier), or the `replace` layout: every sweep
    /// may run.
    Pressure,
}

impl SweepGate {
    /// Whether the sweeps that run before the first request or at the turn boundary may run.
    pub(crate) fn is_open(self) -> bool {
        self != Self::Closed
    }
}

/// Pinned layout, frozen prompt bookkeeping, scrub progress and sweep gate of one conversation.
#[derive(Debug, Default)]
pub(crate) struct LayoutState {
    pinned: Option<PinnedLayout>,
    pub(super) pending_logged: bool,
    pub(super) rendered: Option<SessionPromptKey>,
    pub(super) prompt_inputs: Option<PromptInputs>,
    pub(super) scrub_progress: Option<ScrubProgress>,
    /// Skills whose body was sent, with the trust level they had then.
    pub(super) skill_sent: HashMap<String, SkillTrustLevel>,
    sweep_gate: SweepGate,
    mid_swept_this_turn: bool,
    last_sweep_tokens: Option<u64>,
    /// Directive keys present in history when this turn's directives were built.
    pub(super) directive_basis: Option<HashSet<RetrievalKey>>,
    /// Inputs of this turn's directives, kept to rebuild them after a same-turn boundary.
    pub(super) turn_inputs: Option<TurnInputs>,
}

impl LayoutState {
    /// The layout this conversation is pinned to, if a turn already pinned it.
    pub(crate) fn pinned(&self) -> Option<PinnedLayout> {
        self.pinned
    }

    /// `true` when the conversation is pinned to the `append` layout.
    pub(crate) fn is_append(&self) -> bool {
        self.pinned.is_some_and(PinnedLayout::is_append)
    }

    /// Forget the pin (a new conversation re-pins on its first turn).
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Re-render the frozen prompt (history trim, hard compaction, focus).
    pub(crate) fn invalidate_prompt(&mut self) {
        self.rendered = None;
    }

    /// Forget the scrub progress so the next request scrubs the whole window once.
    pub(crate) fn invalidate_scrub_marker(&mut self) {
        self.scrub_progress = None;
    }

    /// Decide whether this turn's sweeps may rewrite old messages.
    ///
    /// `append` sweeps once when the context enters the soft compaction band, then only after
    /// the prompt has grown by [`SWEEP_REARM_PERCENT`] over the size recorded after the last
    /// sweep, or at the hard tier. Below the soft threshold the gate re-arms. An
    /// [`Expired`](CacheWarmth::Expired) cache opens the soft band without growth, but only as
    /// [`SweepGate::CacheExpired`]: the turn's first request re-warms the cache, so later
    /// sweeps need real pressure.
    fn decide_sweeps(
        &mut self,
        tier: CompactionTier,
        tokens: u64,
        warmth: CacheWarmth,
    ) -> SweepGate {
        let gate = match tier {
            CompactionTier::None => {
                self.last_sweep_tokens = None;
                SweepGate::Closed
            }
            CompactionTier::Soft => {
                let grown = self.last_sweep_tokens.is_none_or(|last| {
                    tokens.saturating_mul(100) >= last.saturating_mul(SWEEP_REARM_PERCENT)
                });
                if grown {
                    SweepGate::Pressure
                } else if warmth == CacheWarmth::Expired {
                    SweepGate::CacheExpired
                } else {
                    SweepGate::Closed
                }
            }
            _ => SweepGate::Pressure,
        };
        self.sweep_gate = gate;
        self.mid_swept_this_turn = false;
        gate
    }
}

impl<C: Channel> Agent<C> {
    /// Pin the conversation to a layout on its first turn; later turns only report a pending
    /// change (a hot-reloaded `context_layout`, a provider switch) once.
    #[tracing::instrument(name = "core.context.layout_pin", skip_all, level = "debug")]
    pub(in crate::agent) fn ensure_layout_pinned(&mut self) {
        let configured = self.services.memory.compaction.context_layout;
        let strategy = self.services.memory.compaction.context_strategy;
        let wants_append = resolves_to_append(configured, strategy, self.provider.prefix_cache());
        match self.msg.layout.pinned {
            None => {
                let pinned = if wants_append {
                    PinnedLayout::Append {
                        key: ConversationKey(uuid::Uuid::new_v4().as_u64_pair().0),
                    }
                } else {
                    PinnedLayout::Replace
                };
                if configured == ContextLayout::Append && !wants_append {
                    tracing::warn!(
                        ?strategy,
                        "context_layout = append ignored: the context strategy drains history every turn"
                    );
                }
                tracing::info!(
                    layout = pinned.label(),
                    ?configured,
                    "context layout pinned"
                );
                self.msg.layout.pinned = Some(pinned);
                self.msg.layout.pending_logged = false;
            }
            Some(pinned) if pinned.is_append() != wants_append => {
                if !self.msg.layout.pending_logged {
                    self.msg.layout.pending_logged = true;
                    tracing::info!(
                        target: "core.context.layout_pending",
                        pinned = pinned.label(),
                        ?configured,
                        "context layout change pending until /clear"
                    );
                }
            }
            Some(_) => self.msg.layout.pending_logged = false,
        }
    }

    /// Effective layout label for `/status`, with a pending-change notice when it differs from
    /// what the current config and provider would resolve to.
    pub(crate) fn context_layout_status(&self) -> String {
        let configured = self.services.memory.compaction.context_layout;
        let strategy = self.services.memory.compaction.context_strategy;
        let wants_append = resolves_to_append(configured, strategy, self.provider.prefix_cache());
        let label = |append: bool| if append { "append" } else { "replace" };
        match self.msg.layout.pinned() {
            Some(p) if p.is_append() != wants_append => format!(
                "{} (pending: {} after /clear)",
                p.label(),
                label(wants_append)
            ),
            Some(p) => p.label().to_owned(),
            None => label(wants_append).to_owned(),
        }
    }

    /// Decide, once per turn, whether the tool-output sweeps may rewrite old messages.
    ///
    /// Always in the `replace` layout; see [`LayoutState::decide_sweeps`] for `append`.
    #[tracing::instrument(
        name = "core.context.sweep_gate",
        skip_all,
        level = "debug",
        fields(
            allowed = tracing::field::Empty,
            gate = tracing::field::Empty,
            cache = tracing::field::Empty
        )
    )]
    pub(in crate::agent) fn decide_sweeps(&mut self) -> bool {
        let gate = if self.msg.layout.is_append() {
            let tokens = self.runtime.providers.cached_prompt_tokens;
            let tier = self.context_manager.compaction_tier(tokens);
            let warmth = self.cache_warmth();
            tracing::Span::current().record("cache", tracing::field::debug(warmth));
            self.msg.layout.decide_sweeps(tier, tokens, warmth)
        } else {
            self.msg.layout.sweep_gate = SweepGate::Pressure;
            SweepGate::Pressure
        };
        tracing::Span::current().record("gate", tracing::field::debug(gate));
        tracing::Span::current().record("allowed", gate.is_open());
        gate.is_open()
    }

    /// Time since the last assistant reply, the single clock behind every cache-age decision.
    ///
    /// A turn that ends without pushing an assistant message (blocked reply, timeout, cancel)
    /// leaves the clock unmoved although the server cache was refreshed, so the next turn may
    /// read `Expired` on a warm cache. That costs at most one avoidable rewrite and is accepted.
    pub(in crate::agent) fn assistant_idle(&self) -> Option<Duration> {
        self.services
            .session
            .last_assistant_at
            .map(|at| at.elapsed())
    }

    /// Warmth of the provider prefix cache; `Unknown` when the provider reports no TTL.
    pub(in crate::agent) fn cache_warmth(&self) -> CacheWarmth {
        CacheWarmth::classify(self.provider.prefix_cache().ttl(), self.assistant_idle())
    }

    /// Gate for the soft tier of `maybe_compact`.
    ///
    /// Deferred only in the `append` layout while the cache is known warm and the turn's sweep
    /// gate is closed: a soft rewrite would then bust a live cache for no pressure relief the
    /// hysteresis wants yet. Providers without a TTL keep the ungated behavior.
    pub(in crate::agent) fn soft_tier_gate(&self) -> SoftTierGate {
        if self.msg.layout.is_append()
            && !self.sweeps_allowed()
            && self.cache_warmth() == CacheWarmth::Warm
        {
            SoftTierGate::Deferred
        } else {
            SoftTierGate::Open
        }
    }

    /// Whether the sweeps before the first request and at the turn boundary may run: the
    /// decision of [`Self::decide_sweeps`] for the current turn, whatever its reason.
    pub(in crate::agent) fn sweeps_allowed(&self) -> bool {
        self.msg.layout.sweep_gate.is_open()
    }

    /// Whether the sweeps that run after the turn's first request (mid-iteration, post-response
    /// truncation) may run: only under token pressure, never for an expired cache alone.
    pub(in crate::agent) fn pressure_sweeps_allowed(&self) -> bool {
        self.msg.layout.sweep_gate == SweepGate::Pressure
    }

    /// Whether the mid-iteration summary apply and soft prune may rewrite history now.
    ///
    /// `replace` always allows it. `append` allows it at most once per turn, and only when the
    /// turn's sweep gate is open under pressure or the hard tier is reached, so a tool loop
    /// inside the soft band does not rewrite sent history on every iteration.
    pub(in crate::agent) fn mid_iteration_sweep_allowed(&mut self) -> bool {
        if !self.msg.layout.is_append() {
            return true;
        }
        let tier = self
            .context_manager
            .compaction_tier(self.runtime.providers.cached_prompt_tokens);
        // The hard tier is never rationed: a long tool loop must be able to keep shrinking.
        if tier == CompactionTier::Hard {
            tracing::debug!(?tier, "mid-iteration sweep gate: hard tier");
            return true;
        }
        let allowed = !self.msg.layout.mid_swept_this_turn
            && tier != CompactionTier::None
            && self.pressure_sweeps_allowed();
        if allowed {
            self.msg.layout.mid_swept_this_turn = true;
        }
        tracing::debug!(allowed, ?tier, "mid-iteration sweep gate");
        allowed
    }

    /// Apply deferred tool-pair summaries at the turn boundary.
    ///
    /// The count trigger would rewrite sent tool pairs in the `append` layout; there only the
    /// sweep gate (token pressure with hysteresis) may apply them.
    pub(in crate::agent) fn apply_deferred_summaries_for_turn(&mut self) {
        use zeph_agent_context::DeferredApplyTrigger;

        if !self.msg.layout.is_append() {
            self.maybe_apply_deferred_summaries(DeferredApplyTrigger::TokenOrCount);
        } else if self.sweeps_allowed() {
            self.maybe_apply_deferred_summaries(DeferredApplyTrigger::TokenPressure);
        }
    }

    /// Record the prompt size after a sweep so the gate re-arms on growth, not on every turn.
    pub(in crate::agent) fn note_sweep_done(&mut self) {
        if self.msg.layout.is_append() {
            self.msg.layout.last_sweep_tokens = Some(self.runtime.providers.cached_prompt_tokens);
        }
    }

    /// Settings of the `append` layout for the context assembly view; `None` in `replace`.
    pub(in crate::agent) fn append_settings(
        &self,
    ) -> Option<zeph_agent_context::state::AppendSettings> {
        self.msg
            .layout
            .is_append()
            .then_some(zeph_agent_context::state::AppendSettings {
                turn_budget_ratio: self.services.memory.compaction.turn_context_budget_ratio,
                history_watermark: self.services.memory.compaction.history_trim_watermark,
            })
    }

    /// Stamp the request policy and scrub the unsent tail right before a provider call.
    pub(in crate::agent) fn prepare_request_state(&mut self) {
        let policy = self
            .msg
            .layout
            .pinned()
            .and_then(PinnedLayout::request_policy);
        self.msg.stamp_request_policy(policy);
        if policy.is_some() {
            self.scrub_unsent_tail();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: PrefixCache = PrefixCache::Prefix { ttl: None };

    #[test]
    fn append_resolution_follows_config_and_cache_support() {
        use ContextLayout::{Append, Auto, Replace};
        let full = ContextStrategy::FullHistory;
        assert!(resolves_to_append(Auto, full, PREFIX));
        assert!(!resolves_to_append(Auto, full, PrefixCache::None));
        assert!(resolves_to_append(Append, full, PrefixCache::None));
        assert!(!resolves_to_append(Replace, full, PREFIX));
    }

    #[test]
    fn draining_strategies_force_replace() {
        for strategy in [ContextStrategy::MemoryFirst, ContextStrategy::Adaptive] {
            assert!(!resolves_to_append(ContextLayout::Append, strategy, PREFIX));
        }
    }

    #[test]
    fn replace_layout_has_no_request_policy_and_append_is_verbatim() {
        assert!(PinnedLayout::Replace.request_policy().is_none());
        let policy = PinnedLayout::Append {
            key: ConversationKey(9),
        }
        .request_policy()
        .unwrap();
        assert_eq!(policy.thinking, ThinkingScope::Verbatim);
        assert_eq!(policy.conversation, ConversationKey(9));
    }

    #[test]
    fn layout_state_reset_forgets_pin_and_prompt() {
        let mut state = LayoutState {
            pinned: Some(PinnedLayout::Replace),
            skill_sent: HashMap::from([("s".to_owned(), SkillTrustLevel::Trusted)]),
            last_sweep_tokens: Some(5),
            ..LayoutState::default()
        };
        state.reset();
        assert!(state.pinned().is_none());
        assert!(state.skill_sent.is_empty());
        assert!(state.last_sweep_tokens.is_none());
    }

    #[test]
    fn sweep_gate_fires_on_entry_then_only_after_growth() {
        use SweepGate::{Closed, Pressure};
        let mut state = LayoutState::default();
        let decide = |state: &mut LayoutState, tier, tokens| {
            state.decide_sweeps(tier, tokens, CacheWarmth::Unknown)
        };
        assert_eq!(decide(&mut state, CompactionTier::None, 100), Closed);
        assert_eq!(
            decide(&mut state, CompactionTier::Soft, 700),
            Pressure,
            "entering the band sweeps"
        );
        state.last_sweep_tokens = Some(650);
        assert_eq!(
            decide(&mut state, CompactionTier::Soft, 660),
            Closed,
            "no sweep every turn"
        );
        assert_eq!(
            decide(&mut state, CompactionTier::Soft, 720),
            Pressure,
            "10% growth re-arms"
        );
        assert_eq!(
            decide(&mut state, CompactionTier::Hard, 660),
            Pressure,
            "hard tier always sweeps"
        );
        assert_eq!(decide(&mut state, CompactionTier::None, 100), Closed);
        assert!(
            state.last_sweep_tokens.is_none(),
            "dropping below soft re-arms"
        );
    }

    #[test]
    fn expired_cache_opens_only_the_pre_request_sweeps_without_growth() {
        let mut state = LayoutState {
            last_sweep_tokens: Some(650),
            ..LayoutState::default()
        };
        let soft = |state: &mut LayoutState, warmth| {
            state.decide_sweeps(CompactionTier::Soft, 660, warmth)
        };
        assert_eq!(soft(&mut state, CacheWarmth::Warm), SweepGate::Closed);
        assert_eq!(soft(&mut state, CacheWarmth::Unknown), SweepGate::Closed);
        assert_eq!(
            soft(&mut state, CacheWarmth::Expired),
            SweepGate::CacheExpired
        );
        assert_eq!(
            state.decide_sweeps(CompactionTier::None, 100, CacheWarmth::Expired),
            SweepGate::Closed,
            "an expired cache does not sweep below the soft band"
        );
        state.last_sweep_tokens = Some(650);
        assert_eq!(
            state.decide_sweeps(CompactionTier::Soft, 720, CacheWarmth::Expired),
            SweepGate::Pressure,
            "growth wins over expiry"
        );
        assert_eq!(
            state.decide_sweeps(CompactionTier::Hard, 660, CacheWarmth::Warm),
            SweepGate::Pressure
        );
    }

    #[test]
    fn warmth_needs_both_ttl_and_idle_time() {
        let ttl = Some(Duration::from_mins(5));
        let idle = |mins| Some(Duration::from_mins(mins));
        assert_eq!(CacheWarmth::classify(ttl, idle(4)), CacheWarmth::Warm);
        assert_eq!(CacheWarmth::classify(ttl, idle(5)), CacheWarmth::Expired);
        assert_eq!(CacheWarmth::classify(None, idle(60)), CacheWarmth::Unknown);
        assert_eq!(CacheWarmth::classify(ttl, None), CacheWarmth::Unknown);
    }
}

#[cfg(test)]
mod agent_tests {
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{Message, MessagePart, Role, TurnContextKind};

    use super::*;
    use crate::agent::agent_tests::{MockChannel, MockToolExecutor, create_test_registry};

    fn agent_with(layout: ContextLayout) -> Agent<MockChannel> {
        let mut agent = Agent::new(
            AnyProvider::Mock(MockProvider::with_responses(vec![])),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.services.memory.compaction.context_layout = layout;
        agent
    }

    #[test]
    fn auto_resolves_to_replace_for_a_provider_without_prefix_cache() {
        let mut agent = agent_with(ContextLayout::Auto);
        agent.ensure_layout_pinned();
        assert_eq!(agent.msg.layout.pinned(), Some(PinnedLayout::Replace));
    }

    #[test]
    fn append_pins_with_a_conversation_key_and_clear_repins() {
        let mut agent = agent_with(ContextLayout::Append);
        agent.ensure_layout_pinned();
        let first = agent.msg.layout.pinned().unwrap();
        assert!(first.is_append());

        agent.ensure_layout_pinned();
        assert_eq!(
            agent.msg.layout.pinned(),
            Some(first),
            "pin must not change"
        );

        agent.clear_history();
        assert!(agent.msg.layout.pinned().is_none());
        agent.services.memory.compaction.context_layout = ContextLayout::Replace;
        agent.ensure_layout_pinned();
        assert_eq!(agent.msg.layout.pinned(), Some(PinnedLayout::Replace));
    }

    #[test]
    fn config_change_is_pending_until_clear() {
        let mut agent = agent_with(ContextLayout::Append);
        agent.ensure_layout_pinned();
        agent.services.memory.compaction.context_layout = ContextLayout::Replace;
        agent.ensure_layout_pinned();
        assert!(
            agent.msg.layout.is_append(),
            "running conversation keeps its layout"
        );
        assert!(agent.msg.layout.pending_logged);
        assert!(agent.context_layout_status().contains("pending: replace"));
    }

    #[test]
    fn memory_first_strategy_forces_replace() {
        let mut agent = agent_with(ContextLayout::Append);
        agent.services.memory.compaction.context_strategy = ContextStrategy::MemoryFirst;
        agent.ensure_layout_pinned();
        assert_eq!(agent.msg.layout.pinned(), Some(PinnedLayout::Replace));
    }

    #[test]
    fn request_policy_is_stamped_only_in_append() {
        let mut agent = agent_with(ContextLayout::Append);
        agent.ensure_layout_pinned();
        agent.prepare_request_state();
        let policy = agent.msg.messages[0].metadata.request_policy.unwrap();
        assert_eq!(policy.thinking, ThinkingScope::Verbatim);

        agent.msg.layout.reset();
        agent.services.memory.compaction.context_layout = ContextLayout::Replace;
        agent.ensure_layout_pinned();
        agent.prepare_request_state();
        assert!(agent.msg.messages[0].metadata.request_policy.is_none());
    }

    #[tokio::test]
    async fn append_prompt_is_frozen_and_directives_are_deduplicated() {
        let mut agent = agent_with(ContextLayout::Append);
        agent.ensure_layout_pinned();

        agent.rebuild_system_prompt("first query").await;
        let frozen = agent.msg.messages[0].content.clone();
        assert!(frozen.contains("<!-- cache:stable -->"));
        assert!(
            !frozen.contains("<!-- cache:volatile -->"),
            "append prompt carries no volatile tail"
        );
        let first = agent.msg.pending_preamble.directives.take().unwrap();
        assert_eq!(first.kind, TurnContextKind::Directives);
        assert!(first.text.contains("<environment_update>"));

        let preamble = zeph_agent_context::state::TurnPreamble {
            directives: Some(first),
            retrieved: None,
        };
        let turn = agent.build_user_message("first query", vec![], preamble);
        assert!(matches!(
            turn.parts.last(),
            Some(MessagePart::TurnContext(_))
        ));
        assert_eq!(
            turn.content, "first query",
            "TurnContext stays out of content"
        );
        agent.msg.messages.push(turn);

        agent.rebuild_system_prompt("second query").await;
        assert_eq!(
            agent.msg.messages[0].content, frozen,
            "prompt must stay byte-identical"
        );
        let second = agent.msg.pending_preamble.directives.take();
        assert!(
            second.is_none_or(|b| !b.text.contains("<environment_update>")),
            "an environment already in history must not be resent"
        );
    }

    #[tokio::test]
    async fn invalidating_the_prompt_re_renders_it_on_the_next_turn() {
        let mut agent = agent_with(ContextLayout::Append);
        agent.ensure_layout_pinned();
        agent.rebuild_system_prompt("q").await;
        agent.msg.messages[0].content = "tampered".to_owned();
        agent.rebuild_system_prompt("q").await;
        assert_eq!(
            agent.msg.messages[0].content, "tampered",
            "no re-render without a boundary"
        );

        agent.msg.layout.invalidate_prompt();
        agent.rebuild_system_prompt("q").await;
        assert_ne!(agent.msg.messages[0].content, "tampered");
    }

    #[tokio::test]
    async fn replace_layout_keeps_rebuilding_with_a_volatile_tail() {
        let mut agent = agent_with(ContextLayout::Replace);
        agent.ensure_layout_pinned();
        agent.rebuild_system_prompt("q").await;
        assert!(
            agent.msg.messages[0]
                .content
                .contains("<!-- cache:volatile -->")
        );
        assert!(agent.msg.pending_preamble.directives.is_none());
    }

    #[test]
    fn sweeps_run_in_replace_and_are_gated_in_append() {
        let mut agent = agent_with(ContextLayout::Replace);
        agent.ensure_layout_pinned();
        assert!(agent.decide_sweeps());
        assert!(agent.sweeps_allowed());

        agent.msg.layout.reset();
        agent.services.memory.compaction.context_layout = ContextLayout::Append;
        agent.ensure_layout_pinned();
        agent.context_manager.budget = Some(zeph_context::budget::ContextBudget::new(1_000, 0.2));
        agent.runtime.providers.cached_prompt_tokens = 10;
        assert!(!agent.decide_sweeps());
        assert!(!agent.sweeps_allowed());

        agent.runtime.providers.cached_prompt_tokens = 700;
        assert!(agent.decide_sweeps(), "entering the soft band sweeps once");
        agent.runtime.providers.cached_prompt_tokens = 650;
        agent.note_sweep_done();
        agent.runtime.providers.cached_prompt_tokens = 660;
        assert!(!agent.decide_sweeps(), "the next turn in the band does not");
    }

    /// Cache lifetimes that avoid subtracting from `Instant::now()` (panics on short-uptime
    /// Windows runners): a day is never reached within a test, zero is always reached.
    const NEVER_EXPIRES: Duration = Duration::from_hours(24);

    fn cache_with_ttl(ttl: Option<Duration>) -> PrefixCache {
        PrefixCache::Prefix { ttl }
    }

    fn append_agent_in_soft_band(cache: PrefixCache) -> Agent<MockChannel> {
        let mut agent = Agent::new(
            AnyProvider::Mock(MockProvider::with_responses(vec![]).with_prefix_cache(cache)),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.services.memory.compaction.context_layout = ContextLayout::Append;
        agent.ensure_layout_pinned();
        agent.context_manager.budget = Some(zeph_context::budget::ContextBudget::new(1_000, 0.2));
        agent.runtime.providers.cached_prompt_tokens = 700;
        agent.services.session.last_assistant_at = Some(std::time::Instant::now());
        agent.msg.layout.last_sweep_tokens = Some(690);
        agent
    }

    #[test]
    fn warm_cache_defers_the_soft_tier_until_it_expires() {
        let mut warm = append_agent_in_soft_band(cache_with_ttl(Some(NEVER_EXPIRES)));
        assert!(!warm.decide_sweeps());
        assert_eq!(warm.soft_tier_gate(), SoftTierGate::Deferred);

        let mut expired = append_agent_in_soft_band(cache_with_ttl(Some(Duration::ZERO)));
        assert!(expired.decide_sweeps(), "an expired cache opens the gate");
        assert_eq!(expired.soft_tier_gate(), SoftTierGate::Open);
    }

    #[test]
    fn provider_without_ttl_keeps_the_ungated_soft_tier() {
        let mut agent = append_agent_in_soft_band(cache_with_ttl(None));
        assert!(
            !agent.decide_sweeps(),
            "falls back to the growth hysteresis"
        );
        assert_eq!(agent.cache_warmth(), CacheWarmth::Unknown);
        assert_eq!(agent.soft_tier_gate(), SoftTierGate::Open);
    }

    #[test]
    fn explicit_append_without_a_prefix_cache_never_defers_the_soft_tier() {
        let mut agent = append_agent_in_soft_band(PrefixCache::None);
        assert_eq!(agent.cache_warmth(), CacheWarmth::Unknown);
        assert!(
            !agent.decide_sweeps(),
            "sweeps follow the growth hysteresis"
        );
        assert_eq!(agent.soft_tier_gate(), SoftTierGate::Open);
    }

    #[test]
    fn expired_cache_leaves_mid_iteration_and_post_response_sweeps_closed() {
        let mut agent = append_agent_in_soft_band(cache_with_ttl(Some(Duration::ZERO)));
        assert!(agent.decide_sweeps());
        assert_eq!(agent.msg.layout.sweep_gate, SweepGate::CacheExpired);
        assert!(agent.sweeps_allowed(), "pre-request sweeps may run");
        assert!(
            !agent.pressure_sweeps_allowed(),
            "post-response truncation needs pressure"
        );
        assert!(
            !agent.mid_iteration_sweep_allowed(),
            "the turn's first request re-warmed the cache"
        );
    }

    #[test]
    fn pressure_keeps_the_mid_iteration_sweep_once_per_turn() {
        let mut agent = append_agent_in_soft_band(cache_with_ttl(Some(NEVER_EXPIRES)));
        agent.runtime.providers.cached_prompt_tokens = 800;
        assert!(agent.decide_sweeps());
        assert_eq!(agent.msg.layout.sweep_gate, SweepGate::Pressure);
        assert!(agent.pressure_sweeps_allowed());
        assert!(agent.mid_iteration_sweep_allowed());
        assert!(
            !agent.mid_iteration_sweep_allowed(),
            "at most once per turn"
        );
    }

    #[test]
    fn hard_tier_mid_iteration_sweep_is_never_rationed_for_an_expired_cache() {
        let mut agent = append_agent_in_soft_band(cache_with_ttl(Some(Duration::ZERO)));
        agent.runtime.providers.cached_prompt_tokens = 2_000;
        assert!(agent.decide_sweeps());
        assert!(agent.mid_iteration_sweep_allowed());
        assert!(agent.mid_iteration_sweep_allowed());
    }

    #[test]
    fn replace_layout_never_defers_the_soft_tier() {
        let mut agent = agent_with(ContextLayout::Replace);
        agent.ensure_layout_pinned();
        agent.decide_sweeps();
        assert_eq!(agent.soft_tier_gate(), SoftTierGate::Open);
    }

    #[test]
    fn request_state_scrubs_the_unsent_tail_only_in_append() {
        let mut agent = agent_with(ContextLayout::Append);
        agent.runtime.config.redact_credentials = true;
        agent.ensure_layout_pinned();
        agent.msg.messages.push(Message::from_legacy(
            Role::User,
            "use key sk-abc123def456 now",
        ));
        agent.prepare_request_state();
        assert!(!agent.msg.messages[1].content.contains("sk-abc123def456"));
    }

    async fn agent_with_summary(layout: ContextLayout) -> Agent<MockChannel> {
        agent_with_summary_and_budget(layout, 10_000).await
    }

    async fn agent_with_summary_and_budget(
        layout: ContextLayout,
        budget: usize,
    ) -> Agent<MockChannel> {
        use zeph_memory::semantic::SemanticMemory;

        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            AnyProvider::Mock(MockProvider::default()),
            "test-model",
        )
        .await
        .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();
        memory
            .sqlite()
            .save_summary(cid, "earlier work on project x", None, None, 5)
            .await
            .unwrap();
        let mut agent = agent_with(layout)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
            .with_context_budget(budget, 0.20, 0.80, 4, 0);
        agent.ensure_layout_pinned();
        agent
    }

    #[tokio::test]
    async fn append_collects_retrieved_context_once_and_leaves_history_untouched() {
        let mut agent = agent_with_summary(ContextLayout::Append).await;
        let len_before = agent.msg.messages.len();

        agent.prepare_context("project x").await.unwrap();
        let retrieved = agent.msg.pending_preamble.retrieved.take().unwrap();
        assert_eq!(retrieved.kind, TurnContextKind::Retrieved);
        assert!(retrieved.text.contains("earlier work on project x"));
        assert_eq!(retrieved.keys.len(), 1, "one summary item");
        assert_eq!(
            agent.msg.messages.len(),
            len_before,
            "nothing is injected at the floor"
        );

        let preamble = zeph_agent_context::state::TurnPreamble {
            directives: None,
            retrieved: Some(retrieved),
        };
        let turn = agent.build_user_message("project x", vec![], preamble);
        agent.msg.messages.push(turn);

        agent.prepare_context("project x").await.unwrap();
        assert!(
            agent.msg.pending_preamble.retrieved.is_none(),
            "an item already in history must not be sent again"
        );
    }

    #[tokio::test]
    async fn replace_layout_still_injects_at_the_floor() {
        let mut agent = agent_with_summary(ContextLayout::Replace).await;
        agent
            .msg
            .messages
            .push(Message::from_legacy(Role::User, "hello"));
        agent.prepare_context("project x").await.unwrap();
        assert!(agent.msg.pending_preamble.retrieved.is_none());
        assert!(
            agent
                .msg
                .messages
                .iter()
                .any(|m| m.content.contains("earlier work on project x")),
            "replace keeps the legacy head injection"
        );
    }

    #[tokio::test]
    async fn append_turns_extend_history_without_rewriting_earlier_messages() {
        let mut agent = Agent::new(
            AnyProvider::Mock(MockProvider::with_responses(vec![
                "first answer".to_owned(),
                "second answer".to_owned(),
            ])),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.services.memory.compaction.context_layout = ContextLayout::Append;

        agent
            .process_user_message("first question".to_owned(), vec![])
            .await
            .unwrap();
        let after_first: Vec<String> = agent
            .msg
            .messages
            .iter()
            .map(|m| serde_json::to_string(&(&m.role, &m.content, &m.parts)).unwrap())
            .collect();
        assert!(agent.msg.layout.is_append());
        assert!(
            agent.msg.messages[1]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::TurnContext(_))),
            "the turn input carries its directives"
        );

        agent
            .process_user_message("second question".to_owned(), vec![])
            .await
            .unwrap();
        let after_second: Vec<String> = agent
            .msg
            .messages
            .iter()
            .map(|m| serde_json::to_string(&(&m.role, &m.content, &m.parts)).unwrap())
            .collect();
        assert!(after_second.len() > after_first.len());
        assert!(
            agent.msg.messages[1..]
                .iter()
                .all(|m| m.role != Role::System),
            "no turn-scoped System message may follow index 0"
        );
        assert_eq!(
            after_second[..after_first.len()],
            after_first[..],
            "turn 2 must be a strict extension of turn 1, system prompt included"
        );
    }

    fn tool_pair(agent: &mut Agent<MockChannel>, label: &str, summary: &str) {
        agent.msg.messages.push(Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolUse {
                id: format!("id_{label}"),
                name: label.to_owned(),
                input: serde_json::json!({"cmd": "echo"}),
            }],
        ));
        let mut result = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: format!("id_{label}"),
                content: format!("output of {label}"),
                is_error: false,
            }],
        );
        result.metadata.deferred_summary = Some(summary.to_owned());
        agent.msg.messages.push(result);
    }

    fn has_summary_part(msg: &Message) -> bool {
        msg.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Summary { .. }))
    }

    fn soft_band_agent(layout: ContextLayout) -> Agent<MockChannel> {
        let mut agent = agent_with(layout).with_context_budget(100_000, 0.20, 0.80, 4, 0);
        agent.context_manager.soft_compaction_threshold = 0.70;
        agent.ensure_layout_pinned();
        agent.runtime.providers.cached_prompt_tokens = 75_000;
        agent.decide_sweeps();
        agent
    }

    #[tokio::test]
    async fn append_tool_loop_in_the_soft_band_rewrites_history_at_most_once_per_turn() {
        let mut agent = soft_band_agent(ContextLayout::Append);
        tool_pair(&mut agent, "a", "s_a");
        tool_pair(&mut agent, "b", "s_b");

        agent.apply_mid_iteration_compaction().await;
        let after_first = agent
            .msg
            .messages
            .iter()
            .filter(|m| has_summary_part(m))
            .count();
        assert!(after_first > 0, "entering the band sweeps once");

        // Further iterations stay in the soft band; sent history must not be rewritten again.
        for label in ["c", "d", "e"] {
            tool_pair(&mut agent, label, "later");
            agent.runtime.providers.cached_prompt_tokens = 75_000;
            let before: Vec<String> = agent
                .msg
                .messages
                .iter()
                .map(|m| serde_json::to_string(&(&m.content, &m.parts)).unwrap())
                .collect();
            agent.apply_mid_iteration_compaction().await;
            let after: Vec<String> = agent
                .msg
                .messages
                .iter()
                .map(|m| serde_json::to_string(&(&m.content, &m.parts)).unwrap())
                .collect();
            assert_eq!(before, after, "iteration {label} rewrote sent history");
        }
    }

    #[tokio::test]
    async fn replace_tool_loop_keeps_sweeping_every_iteration() {
        let mut agent = soft_band_agent(ContextLayout::Replace);
        tool_pair(&mut agent, "a", "s_a");
        agent.apply_mid_iteration_compaction().await;
        let first = agent
            .msg
            .messages
            .iter()
            .filter(|m| has_summary_part(m))
            .count();
        tool_pair(&mut agent, "b", "s_b");
        agent.runtime.providers.cached_prompt_tokens = 75_000;
        agent.apply_mid_iteration_compaction().await;
        let second = agent
            .msg
            .messages
            .iter()
            .filter(|m| has_summary_part(m))
            .count();
        assert!(second > first, "replace behavior is unchanged");
    }

    #[tokio::test]
    async fn hard_tier_is_never_rationed_in_append() {
        let mut agent =
            agent_with(ContextLayout::Append).with_context_budget(100_000, 0.20, 0.80, 4, 0);
        agent.ensure_layout_pinned();
        agent.runtime.providers.cached_prompt_tokens = 10;
        agent.decide_sweeps();
        tool_pair(&mut agent, "a", "s_a");
        agent.runtime.providers.cached_prompt_tokens = 95_000;
        assert!(agent.mid_iteration_sweep_allowed());
        assert!(
            agent.mid_iteration_sweep_allowed(),
            "never rationed at the hard tier"
        );
    }

    #[tokio::test]
    async fn directives_are_rebuilt_after_a_boundary_removed_their_messages() {
        use super::super::directives::{AppendTurnInputs, CatalogEntry, SkillSection};

        let mut agent = agent_with(ContextLayout::Append);
        agent.ensure_layout_pinned();
        agent.services.session.env_context.working_dir = "/work/a".into();
        let section = SkillSection {
            key: zeph_context::input::ItemKeys::key("skill", "s\x000\x00body"),
            name: "s".into(),
            trust: SkillTrustLevel::Trusted,
            text: "<available_skills>s: body</available_skills>".into(),
        };
        let inputs = || AppendTurnInputs {
            catalog_prompt: "catalog".into(),
            catalog_entries: vec![CatalogEntry {
                name: "s".into(),
                trust: SkillTrustLevel::Trusted,
            }],
            mcp_instructions: String::new(),
            matched_mcp_tools: vec![],
            skill_sections: vec![section.clone()],
            skill_fallback_mode: false,
            effective_mode: crate::config::SkillPromptMode::Full,
            time_turn: 0,
        };

        agent.assemble_append_turn(inputs()).await;
        let preamble = std::mem::take(&mut agent.msg.pending_preamble);
        let turn = agent.build_user_message("q1", vec![], preamble);
        agent.msg.messages.push(turn);

        // Turn 2: everything is already in history, so the directives skip it.
        agent.assemble_append_turn(inputs()).await;
        let quiet = agent.msg.pending_preamble.directives.clone();
        assert!(quiet.is_none_or(|b| !b.text.contains("body")));

        // A boundary (trim, compaction) drops the message that carried the skill body.
        agent.msg.messages.truncate(1);
        agent.rebuild_directives_if_history_changed().await;
        let rebuilt = agent.msg.pending_preamble.directives.clone().unwrap();
        assert!(
            rebuilt.text.contains("body"),
            "skill body re-sent in the boundary turn"
        );
        assert!(rebuilt.text.contains("working_directory: /work/a"));
    }

    #[tokio::test]
    async fn trim_does_not_lose_items_the_removed_messages_carried() {
        let mut agent = agent_with_summary_and_budget(ContextLayout::Append, 4_000).await;
        agent
            .msg
            .messages
            .push(Message::from_legacy(Role::User, "warm up"));
        agent.prepare_context("project x").await.unwrap();
        let retrieved = agent.msg.pending_preamble.retrieved.take().unwrap();
        assert!(retrieved.text.contains("earlier work on project x"));

        let preamble = zeph_agent_context::state::TurnPreamble {
            directives: None,
            retrieved: Some(retrieved),
        };
        let turn = agent.build_user_message("project x", vec![], preamble);
        agent.msg.messages.push(turn);
        for i in 0..40 {
            let text = format!("filler {i} {}", "padding words ".repeat(40));
            agent
                .msg
                .messages
                .push(Message::from_legacy(Role::Assistant, &text));
            agent
                .msg
                .messages
                .push(Message::from_legacy(Role::User, &text));
        }
        agent.recompute_prompt_tokens();

        agent.prepare_context("project x").await.unwrap();
        assert!(
            !agent
                .msg
                .messages
                .iter()
                .flat_map(|m| &m.parts)
                .any(|p| matches!(p, MessagePart::TurnContext(_))),
            "the trim must have removed the old turn input"
        );
        let again = agent.msg.pending_preamble.retrieved.take();
        assert!(
            again.is_some_and(|b| b.text.contains("earlier work on project x")),
            "the item must be fetched again after the trim removed its carrier"
        );
    }
}
