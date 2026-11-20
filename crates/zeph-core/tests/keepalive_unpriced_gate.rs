// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::large_futures,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "The test exercises the concrete async path without boxing its future.; Fixture construction failures should fail the test immediately."
)]
#![recursion_limit = "256"]

//! Acceptance tests (tests-first) for #7129 (keepalive fails closed when pings cannot be priced
//! and thinking makes the output cap exceed 1) and a behaviour guard for #7128 (the ping request
//! carries the full history captured at the last real request).

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use zeph_core::cache_keepalive::{KeepaliveState, StopReason};
use zeph_core::channel::{Channel, ChannelError, ChannelMessage};
use zeph_core::metrics::MetricsSnapshot;
use zeph_core::{Agent, InteractionConfig};
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{CacheUsage, KeepaliveRequest, PrefixCache, ResponseUsage};
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

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

fn keepalive_cfg(max_pings: u32) -> InteractionConfig {
    let mut c = zeph_core::config::Config::default();
    c.llm.cache_keepalive.enabled = true;
    c.llm.cache_keepalive.max_consecutive_pings =
        zeph_config::KeepalivePingCap::new(max_pings).unwrap();
    InteractionConfig::from_config(&c)
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

async fn run_collect(
    interaction: InteractionConfig,
    steps: &[(Duration, &str)],
) -> (Pings, KeepaliveState) {
    let (mut agent, pings, rx, _dir) = build(interaction, vec![hit()], steps);
    agent.run().await.expect("agent run");
    let state = rx.borrow().cache_keepalive.clone();
    (pings, state)
}

#[tokio::test(start_paused = true)]
async fn keepalive_unpriced_with_thinking_budget_stops_with_unpriced_and_sends_no_ping() {
    let (pings, state) = run_collect(
        keepalive_cfg(6),
        &[
            (Duration::ZERO, "first"),
            (Duration::ZERO, "/think-tokens 2048"),
            (Duration::ZERO, "second"),
            (LONG_IDLE, "end"),
        ],
    )
    .await;
    assert_eq!(
        pings.lock().unwrap().len(),
        0,
        "unpriced ping must not be sent"
    );
    assert_eq!(state, KeepaliveState::Stopped(StopReason::Unpriced));
}

#[tokio::test(start_paused = true)]
async fn keepalive_unpriced_without_thinking_still_pings_when_output_cap_is_one() {
    let (pings, state) = run_collect(
        keepalive_cfg(6),
        &[(Duration::ZERO, "hi"), (Duration::from_secs(285), "again")],
    )
    .await;
    assert_eq!(pings.lock().unwrap().len(), 1);
    assert_ne!(state, KeepaliveState::Stopped(StopReason::Unpriced));
}

#[tokio::test(start_paused = true)]
async fn keepalive_ping_request_carries_every_prior_turn_in_order() {
    let (pings, _state) = run_collect(
        keepalive_cfg(6),
        &[
            (Duration::ZERO, "turn-one"),
            (Duration::ZERO, "turn-two"),
            (Duration::ZERO, "turn-three"),
            (LONG_IDLE, "end"),
        ],
    )
    .await;
    let pings = pings.lock().unwrap();
    let first = pings
        .first()
        .expect("at least one ping during the idle period");
    let contents: Vec<&str> = first.messages.iter().map(|m| m.content.as_str()).collect();
    let pos = |needle: &str| {
        contents
            .iter()
            .position(|c| c.contains(needle))
            .unwrap_or_else(|| panic!("{needle} missing from ping history: {contents:?}"))
    };
    assert!(pos("turn-one") < pos("turn-two") && pos("turn-two") < pos("turn-three"));
}
