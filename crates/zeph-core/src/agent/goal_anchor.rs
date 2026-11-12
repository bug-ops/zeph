// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Graph anchor of the active goal for the causal-distance signal of five-signal retrieval.
//!
//! The anchor is resolved off the turn path: context assembly only reads the cached result
//! ([`GoalAnchor::poll`]) and, when the cache is stale, hands a [`ResolveTicket`] to the
//! background supervisor. Write-back is compare-and-set on an attempt token, so a late result for
//! a previous goal or attempt can never overwrite newer state (spec 004-18 FR-006).

use std::future::Future;
use std::num::NonZeroU8;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use zeph_common::GoalId;
use zeph_memory::EntityId;

use super::agent_supervisor::TaskClass;
use super::{Agent, Channel};

/// Failures after which resolution stops until the goal changes.
const MAX_FAILURES: u8 = 5;
/// Assemblies after which a `Pending` or `Refreshing` slot whose task never reported back expires.
const STALE_IN_FLIGHT: u64 = 8;
/// Assemblies after which a `Resolved` anchor is revalidated in the background.
const REVALIDATE_AFTER: u64 = 64;

/// Count of context assemblies seen by one [`GoalAnchor`]; the clock for backoff and expiry.
///
/// Counts assemblies, not turns, so it keeps advancing across history clears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AssemblyTick(u64);

impl AssemblyTick {
    fn after(self, assemblies: u64) -> Self {
        Self(self.0 + assemblies)
    }

    fn since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// Monotonic token identifying one resolution attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnchorAttempt(u64);

/// Resolution state of the anchor for the active goal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GoalAnchorState {
    Idle,
    Pending {
        goal_id: GoalId,
        attempt: AnchorAttempt,
        prior_failures: u8,
        spawned_at: AssemblyTick,
    },
    Resolved {
        goal_id: GoalId,
        entity: EntityId,
        resolved_at: AssemblyTick,
    },
    /// Serving the cached `entity` while a revalidation runs.
    Refreshing {
        goal_id: GoalId,
        entity: EntityId,
        attempt: AnchorAttempt,
        spawned_at: AssemblyTick,
    },
    Unresolved {
        goal_id: GoalId,
        failures: NonZeroU8,
        retry_at: AssemblyTick,
    },
    Exhausted {
        goal_id: GoalId,
    },
}

impl GoalAnchorState {
    fn goal_id(&self) -> Option<GoalId> {
        match self {
            Self::Idle => None,
            Self::Pending { goal_id, .. }
            | Self::Resolved { goal_id, .. }
            | Self::Refreshing { goal_id, .. }
            | Self::Unresolved { goal_id, .. }
            | Self::Exhausted { goal_id } => Some(*goal_id),
        }
    }

    fn in_flight_attempt(&self) -> Option<AnchorAttempt> {
        match self {
            Self::Pending { attempt, .. } | Self::Refreshing { attempt, .. } => Some(*attempt),
            _ => None,
        }
    }

    /// State after the `failures`-th consecutive failure, with 1, 2, 4, 8 assembly backoff.
    fn after_failure(goal_id: GoalId, failures: u8, now: AssemblyTick) -> Self {
        match NonZeroU8::new(failures).filter(|n| n.get() < MAX_FAILURES) {
            Some(failures) => Self::Unresolved {
                goal_id,
                failures,
                retry_at: now.after(1_u64 << (failures.get() - 1)),
            },
            None => Self::Exhausted { goal_id },
        }
    }
}

#[derive(Debug)]
struct Inner {
    state: GoalAnchorState,
    now: AssemblyTick,
    next_attempt: u64,
}

/// Work handed out by [`GoalAnchor::poll`] when a (re)resolution must be started.
#[derive(Debug, Clone)]
pub(crate) struct ResolveTicket {
    goal_id: GoalId,
    goal_text: String,
    attempt: AnchorAttempt,
    prior: GoalAnchorState,
}

/// Result of [`GoalAnchor::poll`].
#[derive(Debug, Default)]
pub(crate) struct AnchorPoll {
    /// Anchor for the active goal, `None` when neutral recall must be used.
    pub(crate) anchor: Option<EntityId>,
    /// Resolution to start in the background, if the cache is stale.
    pub(crate) ticket: Option<ResolveTicket>,
}

/// Shared slot holding the anchor state of the active goal.
///
/// Cloning shares the slot. The lock is plain bookkeeping and never held across `.await`.
#[derive(Debug, Clone)]
pub(crate) struct GoalAnchor {
    inner: Arc<Mutex<Inner>>,
}

impl Default for GoalAnchor {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state: GoalAnchorState::Idle,
                now: AssemblyTick(0),
                next_attempt: 0,
            })),
        }
    }
}

/// Outcome recorded by [`GoalAnchor::settle`].
#[derive(Debug, Clone, Copy)]
enum Outcome {
    Found(EntityId),
    /// The goal matches no entity.
    NotFound,
    /// Timeout, error or abort: nothing is known about the goal.
    Transient,
}

impl GoalAnchor {
    /// Read the cached anchor for the active goal and decide whether to start a resolution.
    ///
    /// `goal` is `(id, text)` of the active goal, `None` when no goal is active (which resets
    /// the slot). Every call advances the slot's clock by one assembly. A `Resolved` anchor is
    /// served while it is revalidated in the background.
    pub(crate) fn poll(&self, goal: Option<(GoalId, &str)>) -> AnchorPoll {
        let mut inner = self.inner.lock();
        inner.now = inner.now.after(1);
        let now = inner.now;
        let Some((goal_id, goal_text)) = goal else {
            inner.state = GoalAnchorState::Idle;
            return AnchorPoll::default();
        };
        if inner.state.goal_id().is_some_and(|id| id != goal_id) {
            inner.state = GoalAnchorState::Idle;
        }
        Self::expire_in_flight(&mut inner.state, goal_id, now);

        let (anchor, prior_failures) = match &inner.state {
            GoalAnchorState::Resolved {
                entity,
                resolved_at,
                ..
            } if now.since(*resolved_at) >= REVALIDATE_AFTER => (Some(*entity), 0),
            GoalAnchorState::Resolved { entity, .. }
            | GoalAnchorState::Refreshing { entity, .. } => {
                return AnchorPoll {
                    anchor: Some(*entity),
                    ticket: None,
                };
            }
            GoalAnchorState::Idle => (None, 0),
            GoalAnchorState::Unresolved {
                failures, retry_at, ..
            } if now >= *retry_at => (None, failures.get()),
            _ => return AnchorPoll::default(),
        };
        let attempt = AnchorAttempt(inner.next_attempt);
        inner.next_attempt += 1;
        let next = match anchor {
            Some(entity) => GoalAnchorState::Refreshing {
                goal_id,
                entity,
                attempt,
                spawned_at: now,
            },
            None => GoalAnchorState::Pending {
                goal_id,
                attempt,
                prior_failures,
                spawned_at: now,
            },
        };
        let prior = std::mem::replace(&mut inner.state, next);
        AnchorPoll {
            anchor,
            ticket: Some(ResolveTicket {
                goal_id,
                goal_text: goal_text.to_owned(),
                attempt,
                prior,
            }),
        }
    }

    /// A task that never reported back (aborted before its first poll) must not wedge the slot.
    fn expire_in_flight(state: &mut GoalAnchorState, goal_id: GoalId, now: AssemblyTick) {
        match state {
            GoalAnchorState::Pending {
                prior_failures,
                spawned_at,
                ..
            } if now.since(*spawned_at) > STALE_IN_FLIGHT => {
                *state = GoalAnchorState::after_failure(goal_id, *prior_failures + 1, now);
            }
            GoalAnchorState::Refreshing {
                entity, spawned_at, ..
            } if now.since(*spawned_at) > STALE_IN_FLIGHT => {
                *state = GoalAnchorState::Resolved {
                    goal_id,
                    entity: *entity,
                    resolved_at: now,
                };
            }
            _ => {}
        }
    }

    /// Undo a ticket whose task was not accepted by the supervisor; the attempt is not counted.
    pub(crate) fn cancel(&self, ticket: ResolveTicket) {
        let mut inner = self.inner.lock();
        if inner.state.goal_id() == Some(ticket.goal_id)
            && inner.state.in_flight_attempt() == Some(ticket.attempt)
        {
            inner.state = ticket.prior;
        }
    }

    /// Compare-and-set write-back: applies only while the slot is still in flight for this
    /// exact goal and attempt.
    fn settle(&self, goal_id: GoalId, attempt: AnchorAttempt, outcome: Outcome) {
        let mut inner = self.inner.lock();
        let now = inner.now;
        if inner.state.goal_id() != Some(goal_id)
            || inner.state.in_flight_attempt() != Some(attempt)
        {
            tracing::debug!(%goal_id, "goal anchor: stale resolution result dropped");
            return;
        }
        let resolved = |entity| GoalAnchorState::Resolved {
            goal_id,
            entity,
            resolved_at: now,
        };
        inner.state = match (&inner.state, outcome) {
            (_, Outcome::Found(entity)) => resolved(entity),
            (GoalAnchorState::Refreshing { entity, .. }, Outcome::Transient) => resolved(*entity),
            (GoalAnchorState::Refreshing { .. }, Outcome::NotFound) => {
                GoalAnchorState::after_failure(goal_id, 1, now)
            }
            (GoalAnchorState::Pending { prior_failures, .. }, _) => {
                GoalAnchorState::after_failure(goal_id, *prior_failures + 1, now)
            }
            (other, _) => other.clone(),
        };
    }

    /// Future that runs `work` under `timeout` and settles the slot.
    ///
    /// The drop guard is created inside the async block, so a future dropped by the supervisor
    /// before its first poll (concurrency overflow) records nothing; once started, an abort
    /// settles as a transient failure.
    pub(crate) fn resolve_with<W>(
        &self,
        ticket: &ResolveTicket,
        timeout: Duration,
        work: W,
    ) -> impl Future<Output = ()> + Send + 'static
    where
        W: Future<Output = Result<Option<EntityId>, zeph_memory::MemoryError>> + Send + 'static,
    {
        let anchor = self.clone();
        let (goal_id, attempt) = (ticket.goal_id, ticket.attempt);
        async move {
            let mut guard = PendingGuard {
                anchor,
                goal_id,
                attempt,
                armed: true,
            };
            let outcome = match tokio::time::timeout(timeout, work).await {
                Ok(Ok(Some(entity))) => Outcome::Found(entity),
                Ok(Ok(None)) => {
                    tracing::debug!("goal anchor: no graph entity matches the goal");
                    Outcome::NotFound
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "goal anchor: resolution failed");
                    Outcome::Transient
                }
                Err(_) => {
                    tracing::warn!("goal anchor: resolution timed out");
                    Outcome::Transient
                }
            };
            guard.finish(outcome);
        }
    }
}

/// Settles an abandoned resolution as a transient failure so no in-flight state outlives its task.
struct PendingGuard {
    anchor: GoalAnchor,
    goal_id: GoalId,
    attempt: AnchorAttempt,
    armed: bool,
}

impl PendingGuard {
    fn finish(&mut self, outcome: Outcome) {
        self.armed = false;
        self.anchor.settle(self.goal_id, self.attempt, outcome);
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.armed {
            self.finish(Outcome::Transient);
        }
    }
}

impl<C: Channel> Agent<C> {
    /// Anchor entity of the active goal for this turn's recall.
    ///
    /// Synchronous: reads the cached resolution and, when stale, starts a background
    /// resolution through the supervisor. Returns `None` (neutral causal signal) while the
    /// anchor is unresolved, when no goal is active, or when graph memory is disabled.
    pub(in crate::agent) fn goal_anchor_for_turn(&mut self) -> Option<EntityId> {
        let memory = self.services.memory.persistence.memory.clone()?;
        if !self.services.memory.extraction.graph_config.enabled {
            return None;
        }
        let snapshot = self
            .services
            .goal_accounting
            .as_ref()
            .and_then(|accounting| accounting.snapshot());
        let slot = self.services.memory.persistence.goal_anchor.clone();
        let poll = slot.poll(snapshot.as_ref().map(|g| (g.id, g.text.as_str())));
        if let Some(ticket) = poll.ticket {
            let timeout = Duration::from_secs(self.runtime.config.timeouts.embedding_seconds);
            let goal_text = ticket.goal_text.clone();
            let work = async move { memory.resolve_goal_anchor(&goal_text).await };
            let fut = slot.resolve_with(&ticket, timeout, work);
            let accepted =
                self.runtime
                    .lifecycle
                    .supervisor
                    .spawn(TaskClass::Enrichment, "goal_anchor", fut);
            if !accepted {
                slot.cancel(ticket);
            }
        }
        poll.anchor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Work = Result<Option<EntityId>, zeph_memory::MemoryError>;

    fn g(n: u8) -> GoalId {
        GoalId::parse(&format!("00000000-0000-0000-0000-{n:012}")).unwrap()
    }

    fn entity(raw: i64) -> EntityId {
        serde_json::from_value(serde_json::json!(raw)).unwrap()
    }

    fn state(slot: &GoalAnchor) -> GoalAnchorState {
        slot.inner.lock().state.clone()
    }

    fn start(slot: &GoalAnchor, goal: GoalId) -> ResolveTicket {
        slot.poll(Some((goal, "text")))
            .ticket
            .expect("a resolution must start")
    }

    fn settle(slot: &GoalAnchor, t: &ResolveTicket, outcome: Outcome) {
        slot.settle(t.goal_id, t.attempt, outcome);
    }

    fn resolved_slot(goal: GoalId, raw: i64) -> GoalAnchor {
        let slot = GoalAnchor::default();
        let t = start(&slot, goal);
        settle(&slot, &t, Outcome::Found(entity(raw)));
        slot
    }

    fn age_until_revalidation(slot: &GoalAnchor, goal: GoalId) -> ResolveTicket {
        for _ in 0..REVALIDATE_AFTER - 1 {
            assert!(slot.poll(Some((goal, "t"))).ticket.is_none());
        }
        slot.poll(Some((goal, "t")))
            .ticket
            .expect("revalidation starts after the interval")
    }

    #[test]
    fn idle_poll_starts_one_resolution_and_marks_pending() {
        let slot = GoalAnchor::default();
        let ticket = start(&slot, g(1));
        assert!(matches!(state(&slot), GoalAnchorState::Pending { .. }));
        assert!(slot.poll(Some((g(1), "text"))).ticket.is_none());
        drop(ticket);
    }

    #[test]
    fn found_entity_is_served_without_new_resolution() {
        let slot = resolved_slot(g(1), 7);
        let poll = slot.poll(Some((g(1), "text")));
        assert_eq!(poll.anchor, Some(entity(7)));
        assert!(poll.ticket.is_none());
    }

    #[test]
    fn no_active_goal_resets_slot() {
        let slot = resolved_slot(g(1), 7);
        assert!(slot.poll(None).anchor.is_none());
        assert_eq!(state(&slot), GoalAnchorState::Idle);
    }

    #[test]
    fn failures_back_off_1_2_4_8_then_exhaust() {
        let slot = GoalAnchor::default();
        let mut ticket = start(&slot, g(1));
        for wait in [1_u64, 2, 4, 8] {
            settle(&slot, &ticket, Outcome::NotFound);
            for _ in 1..wait {
                assert!(
                    slot.poll(Some((g(1), "t"))).ticket.is_none(),
                    "still backing off"
                );
            }
            ticket = slot
                .poll(Some((g(1), "t")))
                .ticket
                .expect("retry after backoff");
        }
        settle(&slot, &ticket, Outcome::NotFound);
        assert!(matches!(state(&slot), GoalAnchorState::Exhausted { .. }));
        for _ in 0..20 {
            assert!(slot.poll(Some((g(1), "t"))).ticket.is_none());
        }
        assert!(
            slot.poll(Some((g(2), "t"))).ticket.is_some(),
            "new goal retries"
        );
    }

    #[test]
    fn stale_attempt_write_is_ignored() {
        let slot = GoalAnchor::default();
        let first = start(&slot, g(1));
        slot.cancel(first.clone());
        let second = start(&slot, g(1));
        settle(&slot, &first, Outcome::Found(entity(1)));
        assert!(matches!(state(&slot), GoalAnchorState::Pending { .. }));
        drop(second);
    }

    #[test]
    fn goal_switch_during_flight_keeps_new_goal_state() {
        let slot = GoalAnchor::default();
        let a = start(&slot, g(10));
        let b = start(&slot, g(11));
        settle(&slot, &a, Outcome::Found(entity(1)));
        assert!(matches!(
            state(&slot),
            GoalAnchorState::Pending { ref goal_id, .. } if *goal_id == g(11)
        ));
        settle(&slot, &b, Outcome::Found(entity(2)));
        assert_eq!(slot.poll(Some((g(11), "t"))).anchor, Some(entity(2)));
    }

    #[test]
    fn cancel_restores_prior_state_without_counting() {
        let slot = GoalAnchor::default();
        let t = start(&slot, g(1));
        slot.cancel(t);
        assert_eq!(state(&slot), GoalAnchorState::Idle);
    }

    #[tokio::test]
    async fn overflow_dropped_future_does_not_count_as_failure() {
        let slot = GoalAnchor::default();
        let t = start(&slot, g(1));
        let fut = slot.resolve_with(&t, Duration::from_secs(1), std::future::pending::<Work>());
        drop(fut);
        slot.cancel(t);
        assert_eq!(state(&slot), GoalAnchorState::Idle);
    }

    #[tokio::test]
    async fn aborted_task_settles_unresolved_never_pending() {
        let slot = GoalAnchor::default();
        let t = start(&slot, g(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let fut = slot.resolve_with(&t, Duration::from_secs(60), async move {
            let _ = started_tx.send(());
            std::future::pending::<Work>().await
        });
        let handle = tokio::spawn(fut);
        started_rx.await.unwrap();
        handle.abort();
        let _ = handle.await;
        assert!(matches!(
            state(&slot),
            GoalAnchorState::Unresolved { failures, .. } if failures.get() == 1
        ));
    }

    #[tokio::test]
    async fn timeout_counts_as_failure() {
        let slot = GoalAnchor::default();
        let t = start(&slot, g(1));
        slot.resolve_with(
            &t,
            Duration::from_millis(10),
            std::future::pending::<Work>(),
        )
        .await;
        assert!(matches!(
            state(&slot),
            GoalAnchorState::Unresolved { failures, .. } if failures.get() == 1
        ));
    }

    #[tokio::test]
    async fn found_result_resolves_slot() {
        let slot = GoalAnchor::default();
        let t = start(&slot, g(1));
        slot.resolve_with(&t, Duration::from_secs(1), async { Ok(Some(entity(9))) })
            .await;
        assert_eq!(slot.poll(Some((g(1), "t"))).anchor, Some(entity(9)));
    }

    #[test]
    fn never_reporting_pending_expires_as_failure() {
        let slot = GoalAnchor::default();
        let _lost = start(&slot, g(1));
        let mut respawned = false;
        for _ in 0..(STALE_IN_FLIGHT + 4) {
            respawned |= slot.poll(Some((g(1), "t"))).ticket.is_some();
        }
        assert!(respawned, "a lost task must not wedge the slot in Pending");
    }

    #[test]
    fn resolved_anchor_is_revalidated_while_still_served() {
        let slot = resolved_slot(g(1), 7);
        let _ticket = age_until_revalidation(&slot, g(1));
        assert!(matches!(state(&slot), GoalAnchorState::Refreshing { .. }));
        let poll = slot.poll(Some((g(1), "t")));
        assert_eq!(poll.anchor, Some(entity(7)), "cached anchor keeps serving");
        assert!(poll.ticket.is_none(), "one revalidation at a time");
    }

    #[test]
    fn refresh_found_different_entity_replaces_anchor() {
        let slot = resolved_slot(g(1), 7);
        let t = age_until_revalidation(&slot, g(1));
        settle(&slot, &t, Outcome::Found(entity(8)));
        assert_eq!(slot.poll(Some((g(1), "t"))).anchor, Some(entity(8)));
    }

    #[test]
    fn refresh_not_found_drops_anchor_to_unresolved() {
        let slot = resolved_slot(g(1), 7);
        let t = age_until_revalidation(&slot, g(1));
        settle(&slot, &t, Outcome::NotFound);
        assert!(matches!(
            state(&slot),
            GoalAnchorState::Unresolved { failures, .. } if failures.get() == 1
        ));
        assert!(slot.poll(Some((g(1), "t"))).anchor.is_none());
    }

    #[test]
    fn refresh_transient_failure_keeps_current_anchor() {
        let slot = resolved_slot(g(1), 7);
        let t = age_until_revalidation(&slot, g(1));
        settle(&slot, &t, Outcome::Transient);
        assert!(matches!(state(&slot), GoalAnchorState::Resolved { .. }));
        assert_eq!(slot.poll(Some((g(1), "t"))).anchor, Some(entity(7)));
    }

    #[tokio::test]
    async fn aborted_refresh_keeps_current_anchor() {
        let slot = resolved_slot(g(1), 7);
        let t = age_until_revalidation(&slot, g(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(slot.resolve_with(&t, Duration::from_secs(60), async move {
            let _ = started_tx.send(());
            std::future::pending::<Work>().await
        }));
        started_rx.await.unwrap();
        handle.abort();
        let _ = handle.await;
        assert!(matches!(state(&slot), GoalAnchorState::Resolved { .. }));
    }

    #[test]
    fn lost_refresh_expires_back_to_resolved() {
        let slot = resolved_slot(g(1), 7);
        let _lost = age_until_revalidation(&slot, g(1));
        for _ in 0..=STALE_IN_FLIGHT {
            slot.poll(Some((g(1), "t")));
        }
        assert!(matches!(state(&slot), GoalAnchorState::Resolved { .. }));
    }

    #[cfg(feature = "sqlite")]
    mod agent {
        use std::sync::Arc;

        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_memory::semantic::SemanticMemory;

        use super::super::*;
        use crate::agent::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use crate::goal::GoalAccounting;

        async fn agent_with_goal(graph_enabled: bool, with_goal: bool) -> Agent<MockChannel> {
            let store = crate::goal::store::in_memory_store().await;
            if with_goal {
                store.create("ship the parser", None, 2000).await.unwrap();
            }
            let accounting = GoalAccounting::new(Arc::new(store));
            accounting.refresh().await.unwrap();
            let memory = SemanticMemory::with_sqlite_backend(
                ":memory:",
                AnyProvider::Mock(MockProvider::default()),
                "m",
                0.7,
                0.3,
            )
            .await
            .unwrap();
            let mut agent = Agent::new(
                mock_provider(vec![]),
                MockChannel::new(vec![]),
                create_test_registry(),
                None,
                5,
                MockToolExecutor::no_tools(),
            );
            agent.services.goal_accounting = Some(Arc::new(accounting));
            agent.services.memory.persistence.memory = Some(Arc::new(memory));
            agent.services.memory.extraction.graph_config.enabled = graph_enabled;
            agent
        }

        fn enrichment_spawned(agent: &Agent<MockChannel>) -> u64 {
            let metrics = agent.runtime.lifecycle.supervisor.metrics_snapshot();
            metrics.classes[TaskClass::Enrichment.index()].spawned
        }

        fn slot_state(agent: &Agent<MockChannel>) -> GoalAnchorState {
            super::state(&agent.services.memory.persistence.goal_anchor)
        }

        #[tokio::test]
        async fn graph_disabled_returns_none_without_spawning() {
            let mut agent = agent_with_goal(false, true).await;
            assert_eq!(agent.goal_anchor_for_turn(), None);
            assert_eq!(enrichment_spawned(&agent), 0);
            assert_eq!(slot_state(&agent), GoalAnchorState::Idle);
        }

        #[tokio::test]
        async fn no_active_goal_returns_none_and_leaves_slot_idle() {
            let mut agent = agent_with_goal(true, false).await;
            assert_eq!(agent.goal_anchor_for_turn(), None);
            assert_eq!(enrichment_spawned(&agent), 0);
            assert_eq!(slot_state(&agent), GoalAnchorState::Idle);
        }

        #[tokio::test]
        async fn active_goal_spawns_one_resolution_and_marks_pending() {
            let mut agent = agent_with_goal(true, true).await;
            assert_eq!(agent.goal_anchor_for_turn(), None);
            assert_eq!(enrichment_spawned(&agent), 1);
            assert!(matches!(
                slot_state(&agent),
                GoalAnchorState::Pending { .. }
            ));
            assert_eq!(agent.goal_anchor_for_turn(), None);
            assert_eq!(enrichment_spawned(&agent), 1, "no duplicate resolution");
        }

        #[tokio::test]
        async fn rejected_spawn_restores_idle_state() {
            let mut agent = agent_with_goal(true, true).await;
            let config = crate::config::TaskSupervisorConfig {
                enrichment_limit: 0,
                ..crate::config::TaskSupervisorConfig::default()
            };
            agent.runtime.lifecycle.supervisor =
                crate::agent::agent_supervisor::BackgroundSupervisor::new(&config, None);
            assert_eq!(agent.goal_anchor_for_turn(), None);
            assert_eq!(enrichment_spawned(&agent), 0);
            assert_eq!(
                slot_state(&agent),
                GoalAnchorState::Idle,
                "an overflow drop must not burn an attempt"
            );
        }
    }
}
