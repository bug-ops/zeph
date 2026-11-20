// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::large_futures,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "The test exercises the concrete async path without boxing its future.; Fixture construction failures should fail the test immediately."
)]
#![recursion_limit = "256"]

//! Acceptance tests (tests-first) for the prompt-cache keepalive service (spec 101, plan r2
//! Track B with the r3/r4 amendments), driven black-box through a real `Agent` on paused
//! tokio time.
//!
//! Assumed test seam (owned by Track B, `zeph-llm/src/mock.rs`):
//! `MockProvider::with_keepalive(usages) -> (Self, Arc<Mutex<Vec<KeepaliveRequest>>>)` records
//! every `keepalive_ping` request and answers `Pinged(usage)` from the script (the last entry
//! repeats). The mock also stores a `set_thinking_budget` value so a ping's provider clone
//! can be inspected with `AnyProvider::current_thinking_budget()`.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use zeph_core::cache_keepalive::{KeepaliveState, StopReason};
use zeph_core::channel::{Channel, ChannelError, ChannelMessage};
use zeph_core::cost::{CostTracker, ModelPricing};
use zeph_core::metrics::MetricsSnapshot;
use zeph_core::{Agent, InteractionConfig};
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{CacheUsage, KeepaliveRequest, PrefixCache, ResponseUsage};
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

const PRICED_MODEL: &str = "priced-model";
const TTL: Duration = Duration::from_secs(300);
const LONG_IDLE: Duration = Duration::from_secs(2_000);

/// Delivers `(delay_before, text)` inputs on paused time, then EOF.
///
/// `recv` must be cancel-safe: the agent races it against a periodic tick and drops the future
/// on every tick, so the step is kept in `pending` with a fixed deadline until it is delivered.
struct TimelineChannel {
    steps: VecDeque<(Duration, String)>,
    pending: Option<(tokio::time::Instant, String)>,
}

impl TimelineChannel {
    fn new(steps: &[(Duration, &str)]) -> Self {
        Self {
            steps: steps.iter().map(|(d, t)| (*d, (*t).to_owned())).collect(),
            pending: None,
        }
    }
}

impl Channel for TimelineChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        if self.pending.is_none() {
            let Some((delay, text)) = self.steps.pop_front() else {
                return Ok(None);
            };
            self.pending = Some((tokio::time::Instant::now() + delay, text));
        }
        if let Some((deadline, _)) = &self.pending {
            tokio::time::sleep_until(*deadline).await;
        }
        let (_, text) = self.pending.take().expect("pending step set above");
        Ok(Some(ChannelMessage {
            text,
            attachments: vec![],
            is_guest_context: false,
            is_from_bot: false,
            owner_key: None,
        }))
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "send implements the trait-required async signature while completing synchronously"
    )]
    async fn send(&mut self, _text: &str) -> Result<(), ChannelError> {
        Ok(())
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "send_chunk implements the trait-required async signature while completing synchronously"
    )]
    async fn send_chunk(&mut self, _chunk: &str) -> Result<(), ChannelError> {
        Ok(())
    }

    fn flush_chunks(&mut self) -> impl Future<Output = Result<(), ChannelError>> + Send {
        std::future::ready(Ok(()))
    }
}

struct NoopExecutor;
impl ToolExecutor for NoopExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }
    fn set_skill_env(&self, _env: Option<std::collections::HashMap<String, String>>) {}
    zeph_tools::tool_executor_no_inner_defaults!();
}

fn hit() -> ResponseUsage {
    ResponseUsage {
        input: 10,
        output: 1,
        cache: Some(CacheUsage {
            creation: 0,
            read: 4_000,
        }),
        ..ResponseUsage::default()
    }
}

fn miss() -> ResponseUsage {
    ResponseUsage {
        cache: Some(CacheUsage {
            creation: 0,
            read: 0,
        }),
        ..hit()
    }
}

fn rewrite(creation: u64) -> ResponseUsage {
    ResponseUsage {
        cache: Some(CacheUsage {
            creation,
            read: 4_000,
        }),
        ..hit()
    }
}

fn keepalive_cfg(max_pings: u32) -> InteractionConfig {
    let mut c = zeph_core::config::Config::default();
    c.llm.cache_keepalive.enabled = true;
    c.llm.cache_keepalive.max_consecutive_pings =
        zeph_config::KeepalivePingCap::new(max_pings).unwrap();
    InteractionConfig::from_config(&c)
}

struct Harness {
    pings: Arc<Mutex<Vec<KeepaliveRequest>>>,
    state: KeepaliveState,
}

type Pings = Arc<Mutex<Vec<KeepaliveRequest>>>;

fn build(
    interaction: InteractionConfig,
    usages: Vec<ResponseUsage>,
    steps: &[(Duration, &str)],
) -> (
    Agent<TimelineChannel>,
    Pings,
    watch::Receiver<MetricsSnapshot>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    let (mock, pings) = MockProvider::default()
        .with_name("main")
        .with_model_identifier(PRICED_MODEL)
        .with_prefix_cache(PrefixCache::Prefix { ttl: Some(TTL) })
        .with_keepalive(usages);
    let (tx, rx) = watch::channel(MetricsSnapshot::default());
    let agent = Agent::new(
        AnyProvider::Mock(mock),
        TimelineChannel::new(steps),
        registry,
        None,
        5,
        NoopExecutor,
    )
    .with_interaction_config(interaction)
    .with_metrics(tx);
    (agent, pings, rx, dir)
}

async fn run(
    interaction: InteractionConfig,
    usages: Vec<ResponseUsage>,
    steps: &[(Duration, &str)],
) -> Harness {
    let (mut agent, pings, rx, _dir) = build(interaction, usages, steps);
    agent.run().await.expect("agent run");
    let state = rx.borrow().cache_keepalive.clone();
    Harness { pings, state }
}

impl Harness {
    fn ping_count(&self) -> usize {
        self.pings.lock().unwrap().len()
    }
}

// ---- FR-002: default off ---------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn keepalive_default_config_sends_no_ping_during_long_idle() {
    let h = run(
        InteractionConfig::from_config(&zeph_core::config::Config::default()),
        vec![hit()],
        &[(Duration::ZERO, "hi"), (LONG_IDLE, "again")],
    )
    .await;
    assert_eq!(h.ping_count(), 0);
    assert_eq!(h.state, KeepaliveState::Disabled);
}

// ---- FR-001 / timing -------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn keepalive_pings_once_at_ttl_minus_30s_when_idle() {
    let h = run(
        keepalive_cfg(6),
        vec![hit()],
        &[(Duration::ZERO, "hi"), (Duration::from_secs(285), "again")],
    )
    .await;
    assert_eq!(h.ping_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn keepalive_does_not_ping_before_ttl_minus_30s() {
    let h = run(
        keepalive_cfg(6),
        vec![hit()],
        &[(Duration::ZERO, "hi"), (Duration::from_secs(200), "again")],
    )
    .await;
    assert_eq!(h.ping_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn keepalive_real_request_resets_the_timer() {
    let h = run(
        keepalive_cfg(6),
        vec![hit()],
        &[
            (Duration::ZERO, "a"),
            (Duration::from_secs(200), "b"),
            (Duration::from_secs(200), "c"),
        ],
    )
    .await;
    assert_eq!(h.ping_count(), 0, "each 200s gap is below ttl-30s");
}

// ---- snapshot rules (M1', S1) ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn keepalive_failed_main_request_leaves_no_snapshot_so_no_ping() {
    let dir = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    let (mut failing, pings) = MockProvider::failing()
        .with_prefix_cache(PrefixCache::Prefix { ttl: Some(TTL) })
        .with_keepalive(vec![hit()]);
    failing.fail_chat = true;
    let mut agent = Agent::new(
        AnyProvider::Mock(failing),
        TimelineChannel::new(&[(Duration::ZERO, "hi"), (LONG_IDLE, "again")]),
        registry,
        None,
        5,
        NoopExecutor,
    )
    .with_interaction_config(keepalive_cfg(6));
    let _ = agent.run().await;
    assert!(pings.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn keepalive_ping_request_carries_the_main_request_messages() {
    let dir = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    let (mock, pings) = MockProvider::default()
        .with_prefix_cache(PrefixCache::Prefix { ttl: Some(TTL) })
        .with_keepalive(vec![hit()]);
    let (mock, recorded) = mock.with_recording();
    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        TimelineChannel::new(&[
            (Duration::ZERO, "remember the number 42"),
            (LONG_IDLE, "bye"),
        ]),
        registry,
        None,
        5,
        NoopExecutor,
    )
    .with_interaction_config(keepalive_cfg(1));
    agent.run().await.unwrap();
    let pings = pings.lock().unwrap();
    let first_main = recorded.lock().unwrap()[0].clone();
    assert_eq!(pings[0].messages.len(), first_main.len());
    assert_eq!(
        pings[0].messages.last().unwrap().content,
        first_main.last().unwrap().content
    );
}

// ---- cap and re-arm (M2') -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn keepalive_stops_at_max_consecutive_pings_with_cap_reached() {
    let h = run(
        keepalive_cfg(2),
        vec![hit()],
        &[(Duration::ZERO, "hi"), (LONG_IDLE, "again")],
    )
    .await;
    assert_eq!(h.ping_count(), 2);
}

#[tokio::test(start_paused = true)]
async fn keepalive_cap_reached_state_reported_after_idle_period() {
    let (mut agent, _pings, rx, _dir) = build(
        keepalive_cfg(2),
        vec![hit()],
        &[(Duration::ZERO, "hi"), (LONG_IDLE, "again")],
    );
    // Observe before the second user message arrives: run only until the idle gap elapsed.
    let _ = tokio::time::timeout(Duration::from_mins(25), agent.run()).await;
    assert_eq!(
        rx.borrow().cache_keepalive,
        KeepaliveState::Stopped(StopReason::CapReached)
    );
}

#[tokio::test(start_paused = true)]
async fn keepalive_real_activity_rearms_after_cap_reached() {
    let h = run(
        keepalive_cfg(1),
        vec![hit()],
        &[
            (Duration::ZERO, "hi"),
            (LONG_IDLE, "back"),
            (LONG_IDLE, "again"),
        ],
    )
    .await;
    assert_eq!(
        h.ping_count(),
        2,
        "one ping per idle period, re-armed by the 'back' turn"
    );
}

// ---- terminal stops ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn keepalive_no_cache_hit_is_terminal_even_after_new_activity() {
    let h = run(
        keepalive_cfg(6),
        vec![miss()],
        &[
            (Duration::ZERO, "hi"),
            (LONG_IDLE, "back"),
            (LONG_IDLE, "again"),
        ],
    )
    .await;
    assert_eq!(h.ping_count(), 1);
    assert_eq!(h.state, KeepaliveState::Stopped(StopReason::NoCacheHit));
}

#[tokio::test(start_paused = true)]
async fn keepalive_slash_keepalive_on_leaves_no_cache_hit_stop() {
    let h = run(
        keepalive_cfg(1),
        vec![miss(), hit()],
        &[
            (Duration::ZERO, "hi"),
            (LONG_IDLE, "/keepalive on"),
            (Duration::ZERO, "work"),
            (LONG_IDLE, "end"),
        ],
    )
    .await;
    assert_eq!(h.ping_count(), 2);
}

#[tokio::test(start_paused = true)]
async fn keepalive_prefix_rewritten_above_tolerance_stops_terminally() {
    let h = run(
        keepalive_cfg(6),
        vec![rewrite(513)],
        &[
            (Duration::ZERO, "hi"),
            (LONG_IDLE, "back"),
            (LONG_IDLE, "end"),
        ],
    )
    .await;
    assert_eq!(h.ping_count(), 1);
    assert_eq!(
        h.state,
        KeepaliveState::Stopped(StopReason::PrefixRewritten)
    );
}

#[tokio::test(start_paused = true)]
async fn keepalive_cache_write_at_tolerance_does_not_stop() {
    let h = run(
        keepalive_cfg(2),
        vec![rewrite(512)],
        &[(Duration::ZERO, "hi"), (LONG_IDLE, "end")],
    )
    .await;
    assert_eq!(h.ping_count(), 2);
}

#[tokio::test(start_paused = true)]
async fn keepalive_slash_off_stops_with_user_off_and_no_pings() {
    let h = run(
        keepalive_cfg(6),
        vec![hit()],
        &[
            (Duration::ZERO, "hi"),
            (Duration::ZERO, "/keepalive off"),
            (LONG_IDLE, "end"),
        ],
    )
    .await;
    assert_eq!(h.ping_count(), 0);
    assert_eq!(h.state, KeepaliveState::Stopped(StopReason::UserOff));
}

#[tokio::test(start_paused = true)]
async fn keepalive_provider_naming_another_backend_stops_with_provider_mismatch() {
    let mut c = zeph_core::config::Config::default();
    c.llm.cache_keepalive.enabled = true;
    c.llm.cache_keepalive.keepalive_provider = zeph_common::ProviderName::new("other");
    let h = run(
        InteractionConfig::from_config(&c),
        vec![hit()],
        &[(Duration::ZERO, "hi"), (LONG_IDLE, "end")],
    )
    .await;
    assert_eq!(h.ping_count(), 0);
    assert_eq!(
        h.state,
        KeepaliveState::Stopped(StopReason::ProviderMismatch)
    );
}

// ---- r4: provider clone travels with the snapshot -------------------------------------------------

#[tokio::test(start_paused = true)]
async fn keepalive_think_tokens_then_request_then_idle_pings_with_new_budget_and_no_rewrite() {
    // A thinking ping can only be priced with a tracker; without one the gate fails closed.
    let tracker = CostTracker::new(true, 0.0).with_pricing(
        PRICED_MODEL,
        ModelPricing {
            prompt_cents_per_1k: 1.0,
            completion_cents_per_1k: 0.0,
            cache_read_cents_per_1k: 0.001,
            cache_write_cents_per_1k: 1.25,
        },
    );
    let (agent, pings, rx, _dir) = build(
        keepalive_cfg(1),
        vec![hit()],
        &[
            (Duration::ZERO, "first"),
            (Duration::ZERO, "/think-tokens 2048"),
            (Duration::ZERO, "second"),
            (LONG_IDLE, "end"),
        ],
    );
    let mut agent = agent.with_cost_tracker(tracker);
    agent.run().await.expect("agent run");
    let h = Harness {
        state: rx.borrow().cache_keepalive.clone(),
        pings,
    };
    let pings = h.pings.lock().unwrap();
    assert_eq!(pings.len(), 1);
    assert_eq!(pings[0].provider.current_thinking_budget(), Some(2048));
    drop(pings);
    assert_ne!(
        h.state,
        KeepaliveState::Stopped(StopReason::PrefixRewritten)
    );
}

// ---- teardown and isolation (S2) -----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn keepalive_dropping_the_agent_aborts_its_ping_task() {
    let (mut agent, pings, _rx, _dir) = build(
        keepalive_cfg(6),
        vec![hit()],
        &[(Duration::ZERO, "hi"), (Duration::from_secs(285), "again")],
    );
    agent.run().await.unwrap();
    drop(agent);
    let before = pings.lock().unwrap().len();
    tokio::time::sleep(LONG_IDLE).await;
    assert_eq!(
        pings.lock().unwrap().len(),
        before,
        "no ping after the agent is dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn keepalive_concurrent_agents_both_ping_and_do_not_abort_each_other() {
    let steps: &[(Duration, &str)] = &[(Duration::ZERO, "hi"), (Duration::from_secs(285), "end")];
    let (mut a, pings_a, _ra, _da) = build(keepalive_cfg(6), vec![hit()], steps);
    let (mut b, pings_b, _rb, _db) = build(keepalive_cfg(6), vec![hit()], steps);
    let (ra, rb) = tokio::join!(a.run(), b.run());
    ra.unwrap();
    rb.unwrap();
    assert_eq!(pings_a.lock().unwrap().len(), 1);
    assert_eq!(pings_b.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn keepalive_agent_sharing_a_supervisor_gets_a_unique_task_name_per_agent() {
    let sup = Arc::new(zeph_common::TaskSupervisor::new(
        tokio_util::sync::CancellationToken::new(),
    ));
    let steps: &[(Duration, &str)] = &[(Duration::ZERO, "hi"), (Duration::from_secs(285), "end")];
    let (a, pings_a, _ra, _da) = build(keepalive_cfg(6), vec![hit()], steps);
    let (b, pings_b, _rb, _db) = build(keepalive_cfg(6), vec![hit()], steps);
    let (mut a, mut b) = (
        a.with_task_supervisor(Arc::clone(&sup)),
        b.with_task_supervisor(Arc::clone(&sup)),
    );
    let (ra, rb) = tokio::join!(a.run(), b.run());
    ra.unwrap();
    rb.unwrap();
    assert_eq!(pings_a.lock().unwrap().len(), 1);
    assert_eq!(pings_b.lock().unwrap().len(), 1);
}
