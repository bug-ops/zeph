// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Prompt-cache keepalive service (spec 091).
//!
//! While a session is idle, the provider's prompt cache lapses after its TTL and the next real
//! turn pays a full cache write. This module keeps the cache warm: one supervised task per agent
//! replays the last successful main-loop request as a minimal ping shortly before the TTL ends.
//!
//! The agent captures a [`KeepaliveRequest`](zeph_llm::provider::KeepaliveRequest) snapshot only
//! after a successful main response and marks activity at every turn start; the service never
//! observes provider state directly. Its [`KeepaliveState`] is published through
//! [`MetricsSnapshot`](crate::metrics::MetricsSnapshot).
//!
//! Stop rules:
//! - [`StopReason::CapReached`] and [`StopReason::BudgetExhausted`] limit one idle period, so
//!   real activity re-arms them.
//! - Every other [`StopReason`] is terminal until `/keepalive on` or a provider switch.
//!
//! Cost: before each ping a gate prices the worst case (the whole prefix read at the cache-hit
//! price plus the largest output the ping may generate, see
//! [`keepalive_output_cap`](zeph_llm::provider::keepalive_output_cap)) and refuses to ping when
//! that, added to what earlier pings of the idle period cost, reaches the cache write the pings
//! avoid. Without a cost tracker nothing can be priced, so the gate is open and only
//! `max_consecutive_pings` bounds the spend.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tracing::Instrument as _;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{
    KeepaliveOutcome, KeepaliveRequest, LlmProvider as _, ResponseUsage, keepalive_output_cap,
};

use crate::cost::CostTracker;
use crate::metrics::MetricsSnapshot;

/// Pings are sent this long before the provider's cache TTL ends.
const TTL_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// Largest cache write, in tokens, a ping may report before the prefix counts as rewritten.
///
/// A ping replays a cached request, so it should read the cache. A write above this tolerance
/// means something the ping did not capture changed the prefix, and the service stops with
/// [`StopReason::PrefixRewritten`].
///
/// # Examples
///
/// ```
/// assert_eq!(zeph_core::cache_keepalive::PREFIX_REWRITE_TOLERANCE, 512);
/// ```
pub const PREFIX_REWRITE_TOLERANCE: u64 = 512;

/// Longest a single ping may take before it counts as a failure.
const PING_TIMEOUT: Duration = Duration::from_secs(60);

/// Delay before the first retry after a failed ping; doubles on every consecutive failure.
const FAILURE_BACKOFF_BASE: Duration = Duration::from_secs(15);

/// Why the keepalive service stopped pinging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopReason {
    /// `max_consecutive_pings` was reached; real activity re-arms the service.
    CapReached,
    /// A ping reported no cache read: the cache is not being kept alive.
    NoCacheHit,
    /// A ping wrote more cache than the tolerance: the request prefix changed.
    PrefixRewritten,
    /// The daily cost budget is exhausted, or the pings of this idle period would cost as much as
    /// the cache write they avoid; real activity re-arms the service.
    BudgetExhausted,
    /// `keepalive_provider` names a backend other than the one serving requests, or the
    /// serving backend cannot be pinged.
    ProviderMismatch,
    /// The user turned keepalive off with `/keepalive off`.
    UserOff,
}

/// Observable state of the keepalive service.
///
/// # Examples
///
/// ```
/// use zeph_core::cache_keepalive::{KeepaliveState, StopReason};
///
/// assert_eq!(KeepaliveState::default(), KeepaliveState::Disabled);
/// assert_ne!(
///     KeepaliveState::Stopped(StopReason::CapReached),
///     KeepaliveState::Stopped(StopReason::UserOff)
/// );
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeepaliveState {
    /// Keepalive is not enabled.
    #[default]
    Disabled,
    /// Armed and waiting for the next ping.
    Idle,
    /// A ping is in flight.
    InFlight,
    /// Stopped for the given reason.
    Stopped(StopReason),
}

/// State shared between an agent and its keepalive task.
///
/// The agent writes snapshots and activity marks; the task reads them and reports outcomes.
/// All fields sit behind one short-lived lock that is never held across an `.await`.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    inner: Mutex<Inner>,
    wake: Notify,
}

#[derive(Debug, Default)]
struct Inner {
    snapshot: Option<Arc<KeepaliveRequest>>,
    last_activity: Option<Instant>,
    retry_at: Option<Instant>,
    generation: u64,
    consecutive: u32,
    failures: u32,
    ping_spend_cents: f64,
    pings: u64,
    cost_cents: f64,
    speed: zeph_config::ServiceSpeed,
    state: KeepaliveState,
    user: Option<bool>,
    mismatch: bool,
}

/// What a ping starts from: the snapshot and what the idle period has spent so far.
struct PingStart {
    request: Arc<KeepaliveRequest>,
    spent_cents: f64,
    speed: zeph_config::ServiceSpeed,
}

/// What the service loop does next.
enum Plan {
    Wait,
    PingAt(Instant, u64),
}

/// Outcome of one ping attempt, as the state machine needs it.
#[derive(Clone, Copy)]
enum PingResult {
    Skipped,
    Unsupported,
    Failed,
    Pinged {
        usage: ResponseUsage,
        cost_cents: f64,
    },
}

impl Shared {
    pub(crate) fn state(&self) -> KeepaliveState {
        self.inner.lock().state.clone()
    }

    pub(crate) fn user_override(&self) -> Option<bool> {
        self.inner.lock().user
    }

    /// Leave `Disabled` once keepalive is armed for this session.
    pub(crate) fn arm(&self) {
        let mut g = self.inner.lock();
        if g.state == KeepaliveState::Disabled {
            g.state = if g.mismatch {
                KeepaliveState::Stopped(StopReason::ProviderMismatch)
            } else {
                KeepaliveState::Idle
            };
        }
    }

    /// Record whether `keepalive_provider` disagrees with the serving provider.
    pub(crate) fn set_mismatch(&self, mismatch: bool) {
        let mut g = self.inner.lock();
        g.mismatch = mismatch;
        match (&g.state, mismatch) {
            (KeepaliveState::Idle | KeepaliveState::InFlight, true) => {
                g.state = KeepaliveState::Stopped(StopReason::ProviderMismatch);
            }
            (KeepaliveState::Stopped(StopReason::ProviderMismatch), false) => {
                g.state = KeepaliveState::Idle;
            }
            _ => {}
        }
        drop(g);
        self.wake.notify_one();
    }

    /// Whether a main-loop request should be captured as a snapshot right now.
    pub(crate) fn is_capturing(&self) -> bool {
        matches!(
            self.inner.lock().state,
            KeepaliveState::Idle
                | KeepaliveState::InFlight
                | KeepaliveState::Stopped(StopReason::CapReached | StopReason::BudgetExhausted)
        )
    }

    /// Store the snapshot of a successful main-loop request and restart the ping timer.
    ///
    /// `speed` is the service speed the request was billed at; the cost gate prices pings with it.
    pub(crate) fn capture(&self, request: KeepaliveRequest, speed: zeph_config::ServiceSpeed) {
        let mut g = self.inner.lock();
        let previous = g.snapshot.replace(Arc::new(request));
        g.speed = speed;
        g.mark_activity();
        drop(g);
        // The old snapshot can be large; free it outside the lock.
        drop(previous);
        self.wake.notify_one();
    }

    /// Whether a main-loop request has been captured.
    pub(crate) fn has_snapshot(&self) -> bool {
        self.inner.lock().snapshot.is_some()
    }

    /// The previous snapshot's tool definitions when `tool_defs` equals them, so a tool loop that
    /// resends the same tools every iteration shares one allocation instead of copying them.
    pub(crate) fn reusable_tools(
        &self,
        tool_defs: &[zeph_llm::provider::ToolDefinition],
    ) -> Option<Arc<[zeph_llm::provider::ToolDefinition]>> {
        let g = self.inner.lock();
        let previous = &g.snapshot.as_ref()?.tools;
        let same = previous.len() == tool_defs.len()
            && previous.iter().zip(tool_defs).all(|(a, b)| {
                a.name == b.name
                    && a.description == b.description
                    && a.parameters == b.parameters
                    && a.output_schema == b.output_schema
            });
        same.then(|| Arc::clone(previous))
    }

    /// Forget the snapshot: the conversation it replays no longer exists (`/clear`).
    pub(crate) fn clear_snapshot(&self) {
        let mut g = self.inner.lock();
        let previous = g.snapshot.take();
        g.generation += 1;
        drop(g);
        drop(previous);
        self.wake.notify_one();
    }

    /// Pings sent and their cost in cents, for status displays.
    pub(crate) fn stats(&self) -> (u64, f64) {
        let g = self.inner.lock();
        (g.pings, g.cost_cents)
    }

    /// Real activity (a turn started): restart the timer, cancel any in-flight ping and re-arm
    /// a service that stopped at its cap.
    pub(crate) fn mark_activity(&self) {
        let mut g = self.inner.lock();
        if g.state == KeepaliveState::Disabled {
            return;
        }
        g.mark_activity();
        drop(g);
        self.wake.notify_one();
    }

    /// The main provider was replaced: the snapshot is stale and provider-bound stops clear.
    pub(crate) fn provider_changed(&self) {
        let mut g = self.inner.lock();
        let previous = g.snapshot.take();
        g.generation += 1;
        if matches!(
            g.state,
            KeepaliveState::Stopped(StopReason::NoCacheHit | StopReason::PrefixRewritten)
        ) {
            g.state = KeepaliveState::Idle;
            g.consecutive = 0;
        }
        drop(g);
        drop(previous);
        self.wake.notify_one();
    }

    /// `/keepalive off`: stop until `/keepalive on`.
    pub(crate) fn user_off(&self) {
        let mut g = self.inner.lock();
        g.user = Some(false);
        g.generation += 1;
        g.state = KeepaliveState::Stopped(StopReason::UserOff);
        drop(g);
        self.wake.notify_one();
    }

    /// `/keepalive on`: clear any stop and restart the timer.
    pub(crate) fn user_on(&self) -> KeepaliveState {
        let mut g = self.inner.lock();
        g.user = Some(true);
        g.state = if g.mismatch {
            KeepaliveState::Stopped(StopReason::ProviderMismatch)
        } else {
            KeepaliveState::Idle
        };
        g.mark_activity();
        let state = g.state.clone();
        drop(g);
        self.wake.notify_one();
        state
    }

    fn plan(&self) -> Plan {
        let g = self.inner.lock();
        if g.state != KeepaliveState::Idle {
            return Plan::Wait;
        }
        let Some(snapshot) = g.snapshot.as_ref() else {
            return Plan::Wait;
        };
        let Some(ttl) = snapshot.provider.prefix_cache().ttl() else {
            return Plan::Wait;
        };
        let interval = ping_interval(ttl);
        let deadline = g
            .retry_at
            .unwrap_or_else(|| g.last_activity.unwrap_or_else(Instant::now) + interval);
        Plan::PingAt(deadline, g.generation)
    }

    /// Move to `InFlight` and hand out the snapshot, unless activity raced ahead.
    fn begin_ping(&self, generation: u64) -> Option<PingStart> {
        let mut g = self.inner.lock();
        if g.generation != generation || g.state != KeepaliveState::Idle {
            return None;
        }
        let snapshot = g.snapshot.clone()?;
        g.state = KeepaliveState::InFlight;
        Some(PingStart {
            request: snapshot,
            spent_cents: g.ping_spend_cents,
            speed: g.speed,
        })
    }

    /// A ping was cancelled or discarded because activity marked the clock.
    fn abandon_ping(&self) {
        let mut g = self.inner.lock();
        if g.state == KeepaliveState::InFlight {
            g.state = KeepaliveState::Idle;
        }
    }

    fn stop(&self, generation: u64, reason: StopReason) {
        let mut g = self.inner.lock();
        if g.generation == generation && g.state == KeepaliveState::InFlight {
            g.state = KeepaliveState::Stopped(reason);
        }
    }

    /// Apply a ping's result. Returns `false` when activity raced ahead and it was discarded.
    fn finish_ping(&self, generation: u64, max_pings: u32, result: PingResult) -> bool {
        let mut g = self.inner.lock();
        if g.generation != generation || g.state != KeepaliveState::InFlight {
            return false;
        }
        let now = Instant::now();
        g.state = KeepaliveState::Idle;
        match result {
            PingResult::Skipped => {
                g.last_activity = Some(now);
                g.retry_at = None;
            }
            PingResult::Unsupported => {
                g.state = KeepaliveState::Stopped(StopReason::ProviderMismatch);
            }
            PingResult::Failed => {
                g.consecutive += 1;
                g.failures += 1;
                let backoff = FAILURE_BACKOFF_BASE * 2_u32.saturating_pow(g.failures - 1);
                g.retry_at = Some(now + backoff);
                if g.consecutive >= max_pings {
                    g.state = KeepaliveState::Stopped(StopReason::CapReached);
                }
            }
            PingResult::Pinged { usage, cost_cents } => {
                g.consecutive += 1;
                g.failures = 0;
                g.retry_at = None;
                g.last_activity = Some(now);
                g.ping_spend_cents += cost_cents;
                g.pings += 1;
                g.cost_cents += cost_cents;
                let cache = usage.cache.unwrap_or_default();
                g.state = if cache.read == 0 {
                    KeepaliveState::Stopped(StopReason::NoCacheHit)
                } else if cache.creation > PREFIX_REWRITE_TOLERANCE {
                    KeepaliveState::Stopped(StopReason::PrefixRewritten)
                } else if g.consecutive >= max_pings {
                    KeepaliveState::Stopped(StopReason::CapReached)
                } else {
                    KeepaliveState::Idle
                };
            }
        }
        true
    }
}

impl Inner {
    fn mark_activity(&mut self) {
        self.generation += 1;
        self.last_activity = Some(Instant::now());
        self.retry_at = None;
        self.consecutive = 0;
        self.failures = 0;
        self.ping_spend_cents = 0.0;
        if matches!(
            self.state,
            KeepaliveState::Stopped(StopReason::CapReached | StopReason::BudgetExhausted)
                | KeepaliveState::InFlight
        ) {
            self.state = KeepaliveState::Idle;
        }
    }
}

/// Time between pings: the cache TTL minus a safety margin, halved when the TTL is shorter than
/// the margin itself.
fn ping_interval(ttl: Duration) -> Duration {
    if ttl > TTL_SAFETY_MARGIN {
        ttl.saturating_sub(TTL_SAFETY_MARGIN)
    } else {
        ttl / 2
    }
}

/// Byte-length estimate of the cached prefix, one token per four bytes.
fn estimate_prefix_tokens(request: &KeepaliveRequest) -> u64 {
    let messages: usize = request.messages.iter().map(|m| m.content.len()).sum();
    let tools: usize = request
        .tools
        .iter()
        .map(|t| t.name.as_str().len() + t.description.len() + t.parameters.to_string().len())
        .sum();
    u64::try_from((messages + tools) / 4).unwrap_or(u64::MAX)
}

/// Everything the keepalive task needs besides the shared state.
pub(crate) struct ServiceDeps {
    pub(crate) shared: Arc<Shared>,
    pub(crate) metrics_tx: Option<watch::Sender<MetricsSnapshot>>,
    pub(crate) cost: Option<Arc<CostTracker>>,
    pub(crate) max_pings: u32,
}

impl ServiceDeps {
    fn publish(&self) {
        if let Some(tx) = &self.metrics_tx {
            let spend = self.cost.as_ref().map(|c| c.current_spend());
            tx.send_modify(|m| {
                let (pings, cost_cents) = self.shared.stats();
                m.cache_keepalive = self.shared.state();
                m.cache_keepalive_pings = pings;
                m.cache_keepalive_cost_cents = cost_cents;
                if let Some(spend) = spend {
                    m.cost_spent_cents = spend;
                }
            });
        }
    }

    /// Whether another ping is still worth sending: the daily budget has room and the pings of
    /// this idle period, plus this one at its worst case, stay below the cache write they avoid.
    ///
    /// The worst case is the prefix read at the cache-hit price plus the largest output the ping
    /// may generate (see [`keepalive_output_cap`]), priced at the speed the request was billed at.
    fn gate(
        &self,
        request: &KeepaliveRequest,
        spent_cents: f64,
        speed: zeph_config::ServiceSpeed,
    ) -> Option<StopReason> {
        let tracker = self.cost.as_deref()?;
        if tracker.check_budget().is_err() {
            return Some(StopReason::BudgetExhausted);
        }
        let backend = request.provider.keepalive_backend(request.target.as_ref());
        let model = backend.effective_model_identifier();
        let prefix = estimate_prefix_tokens(request);
        let output = u64::from(keepalive_output_cap(backend.current_thinking_budget()));
        let worst_ping = tracker.price_of(model, 0, prefix, 0, output, speed);
        let avoided_write = tracker.price_of(model, 0, 0, prefix, 0, speed);
        let total = spent_cents + worst_ping;
        (total > 0.0 && total >= avoided_write).then_some(StopReason::BudgetExhausted)
    }

    fn record_cost(&self, backend: &AnyProvider, usage: ResponseUsage) -> f64 {
        self.cost.as_deref().map_or(0.0, |tracker| {
            tracker.record_keepalive_usage(
                backend.name(),
                backend.provider_kind_str(),
                backend.effective_model_identifier(),
                usage,
            )
        })
    }

    async fn ping(&self, generation: u64) {
        let Some(PingStart {
            request,
            spent_cents,
            speed,
        }) = self.shared.begin_ping(generation)
        else {
            return;
        };
        if let Some(reason) = self.gate(&request, spent_cents, speed) {
            tracing::warn!(?reason, "cache keepalive stopped by cost gate");
            self.shared.stop(generation, reason);
            self.publish();
            return;
        }
        self.publish();
        // The backend that is actually pinged: for a router, the model, price and thinking
        // budget that count are that backend's, not those of the last request it served.
        let backend = request.provider.keepalive_backend(request.target.as_ref());
        let span = tracing::info_span!(
            "llm.cache.keepalive",
            provider = backend.name(),
            cache_read = tracing::field::Empty,
            cache_creation = tracing::field::Empty,
        );
        // The cache counters are recorded inside the span, before it closes, so log and trace
        // layers that print fields at close or on the span event see them.
        let ping = async {
            let result =
                tokio::time::timeout(PING_TIMEOUT, request.provider.keepalive_ping(&request)).await;
            if let Ok(Ok(KeepaliveOutcome::Pinged(usage))) = &result {
                let cache = usage.cache.unwrap_or_default();
                tracing::Span::current().record("cache_read", cache.read);
                tracing::Span::current().record("cache_creation", cache.creation);
            }
            result
        }
        .instrument(span);
        let outcome = tokio::select! {
            r = ping => Some(r),
            () = self.shared.wake.notified() => None,
        };
        let Some(outcome) = outcome else {
            self.shared.abandon_ping();
            self.publish();
            return;
        };
        let result = match outcome {
            Err(_elapsed) => {
                tracing::warn!("cache keepalive ping timed out, backing off");
                PingResult::Failed
            }
            Ok(Ok(KeepaliveOutcome::Pinged(usage))) => {
                let cost_cents = self.record_cost(backend, usage);
                PingResult::Pinged { usage, cost_cents }
            }
            Ok(Ok(KeepaliveOutcome::Skipped(_))) => PingResult::Skipped,
            Ok(Ok(_)) => {
                tracing::warn!("cache keepalive unsupported by the serving provider");
                PingResult::Unsupported
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "cache keepalive ping failed, backing off");
                PingResult::Failed
            }
        };
        if !self.shared.finish_ping(generation, self.max_pings, result) {
            return;
        }
        self.publish();
        if let KeepaliveState::Stopped(
            reason @ (StopReason::NoCacheHit | StopReason::PrefixRewritten),
        ) = self.shared.state()
        {
            tracing::warn!(?reason, "cache keepalive stopped");
        }
    }
}

/// The keepalive task body: sleeps until the next ping is due and re-plans on every wake.
pub(crate) async fn run_service(deps: ServiceDeps) {
    loop {
        match deps.shared.plan() {
            Plan::Wait => deps.shared.wake.notified().await,
            Plan::PingAt(deadline, generation) => {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline) => deps.ping(generation).await,
                    () = deps.shared.wake.notified() => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeph_llm::any::AnyProvider;
    use zeph_llm::claude::ClaudeProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{CacheUsage, Message, PrefixCache, Role};

    use super::*;
    use crate::cost::ModelPricing;

    const TTL: Duration = Duration::from_secs(300);
    const STANDARD: zeph_config::ServiceSpeed = zeph_config::ServiceSpeed::Standard;

    fn request_with(provider: AnyProvider, text: &str) -> KeepaliveRequest {
        KeepaliveRequest {
            provider,
            messages: Arc::from(vec![Message::from_legacy(Role::User, text)]),
            tools: Arc::from(Vec::new()),
            target: None,
        }
    }

    fn priced_mock(model: &str) -> AnyProvider {
        let mut mock =
            MockProvider::default().with_prefix_cache(PrefixCache::Prefix { ttl: Some(TTL) });
        mock.model_identifier_override = Some(model.to_owned());
        // `with_keepalive` also makes the mock accept a thinking budget.
        let (mock, _) = mock.with_keepalive(vec![]);
        AnyProvider::Mock(mock)
    }

    fn deps(shared: &Arc<Shared>, cost: Option<CostTracker>) -> ServiceDeps {
        ServiceDeps {
            shared: Arc::clone(shared),
            metrics_tx: None,
            cost: cost.map(Arc::new),
            max_pings: 6,
        }
    }

    fn armed(request: KeepaliveRequest) -> Arc<Shared> {
        let shared = Arc::new(Shared::default());
        shared.arm();
        shared.capture(request, zeph_config::ServiceSpeed::default());
        shared
    }

    fn pricing(read: f64, write: f64) -> ModelPricing {
        ModelPricing {
            prompt_cents_per_1k: 1.0,
            completion_cents_per_1k: 1.0,
            cache_read_cents_per_1k: read,
            cache_write_cents_per_1k: write,
        }
    }

    fn hit(creation: u64, read: u64) -> PingResult {
        PingResult::Pinged {
            usage: ResponseUsage {
                cache: Some(CacheUsage { creation, read }),
                ..ResponseUsage::default()
            },
            cost_cents: 0.0,
        }
    }

    #[test]
    fn ping_interval_is_ttl_minus_margin_or_half_for_short_ttls() {
        assert_eq!(
            ping_interval(Duration::from_secs(300)),
            Duration::from_secs(270)
        );
        assert_eq!(
            ping_interval(Duration::from_secs(20)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn cost_gate_passes_while_pings_stay_below_the_avoided_write() {
        let mut tracker = CostTracker::new(true, 0.0);
        tracker = tracker.with_pricing("m", pricing(0.1, 1.25));
        let req = request_with(priced_mock("m"), &"x".repeat(40_000));
        let shared = Arc::new(Shared::default());
        assert_eq!(deps(&shared, Some(tracker)).gate(&req, 0.0, STANDARD), None);
    }

    #[test]
    fn cost_gate_stops_when_pings_would_cost_more_than_the_write_they_avoid() {
        let tracker = CostTracker::new(true, 0.0).with_pricing("m", pricing(0.1, 1.25));
        let req = request_with(priced_mock("m"), &"x".repeat(40_000));
        let shared = Arc::new(Shared::default());
        let d = deps(&shared, Some(tracker));
        // 10_000 prefix tokens: a write avoided costs 12.5 cents.
        assert_eq!(
            d.gate(&req, 12.0, STANDARD),
            Some(StopReason::BudgetExhausted)
        );
    }

    #[test]
    fn cost_gate_stops_when_a_read_costs_more_than_a_write() {
        let tracker = CostTracker::new(true, 0.0).with_pricing("m", pricing(2.0, 1.0));
        let req = request_with(priced_mock("m"), &"x".repeat(40_000));
        let shared = Arc::new(Shared::default());
        assert_eq!(
            deps(&shared, Some(tracker)).gate(&req, 0.0, STANDARD),
            Some(StopReason::BudgetExhausted)
        );
    }

    #[test]
    fn cost_gate_respects_the_daily_budget() {
        let tracker = CostTracker::new(true, 1.0).with_pricing("m", pricing(0.1, 1.25));
        tracker.record_usage(
            "p",
            "cloud",
            "m",
            10_000,
            0,
            0,
            0,
            zeph_config::ServiceSpeed::default(),
        );
        let req = request_with(priced_mock("m"), &"x".repeat(40_000));
        let shared = Arc::new(Shared::default());
        assert_eq!(
            deps(&shared, Some(tracker)).gate(&req, 0.0, STANDARD),
            Some(StopReason::BudgetExhausted)
        );
    }

    fn thinking_priced_request(budget: Option<u32>) -> KeepaliveRequest {
        // Prompt 1 cent, completion 5 cents, read 0.1, write 1.25 per 1k tokens: a 20k-token
        // prefix avoids a 25 cent write, an 8193-token thinking ping can cost 41 cents.
        let mut provider = priced_mock("m");
        provider.set_thinking_budget(budget).unwrap();
        request_with(provider, &"x".repeat(80_000))
    }

    fn thinking_tracker() -> CostTracker {
        CostTracker::new(true, 0.0).with_pricing(
            "m",
            ModelPricing {
                prompt_cents_per_1k: 1.0,
                completion_cents_per_1k: 5.0,
                cache_read_cents_per_1k: 0.1,
                cache_write_cents_per_1k: 1.25,
            },
        )
    }

    #[test]
    fn cost_gate_prices_the_worst_case_thinking_output() {
        let shared = Arc::new(Shared::default());
        let d = deps(&shared, Some(thinking_tracker()));
        assert_eq!(
            d.gate(&thinking_priced_request(Some(8192)), 0.0, STANDARD),
            Some(StopReason::BudgetExhausted),
            "8193 output tokens at 5 cents per 1k exceed the 25 cent write avoided"
        );
        assert_eq!(
            d.gate(&thinking_priced_request(None), 0.0, STANDARD),
            None,
            "one output token keeps the ping far below the write avoided"
        );
    }

    #[test]
    fn budget_exhausted_is_cleared_by_the_next_activity() {
        let shared = armed(request_with(priced_mock("m"), "hi"));
        let generation = shared.inner.lock().generation;
        shared.begin_ping(generation).unwrap();
        shared.stop(generation, StopReason::BudgetExhausted);
        assert_eq!(
            shared.state(),
            KeepaliveState::Stopped(StopReason::BudgetExhausted)
        );
        assert!(shared.is_capturing());
        shared.mark_activity();
        assert_eq!(shared.state(), KeepaliveState::Idle);
        assert!(shared.inner.lock().ping_spend_cents.abs() < f64::EPSILON);
    }

    #[test]
    fn clear_snapshot_drops_the_replayable_history() {
        let shared = armed(request_with(priced_mock("m"), "old history"));
        shared.clear_snapshot();
        assert!(shared.inner.lock().snapshot.is_none());
        assert!(matches!(shared.plan(), Plan::Wait));
    }

    #[test]
    fn stats_count_pings_and_their_cost() {
        let shared = armed(request_with(priced_mock("m"), "hi"));
        let generation = shared.inner.lock().generation;
        shared.begin_ping(generation).unwrap();
        shared.finish_ping(
            generation,
            6,
            PingResult::Pinged {
                usage: ResponseUsage {
                    cache: Some(CacheUsage {
                        creation: 0,
                        read: 10,
                    }),
                    ..ResponseUsage::default()
                },
                cost_cents: 0.5,
            },
        );
        assert_eq!(shared.stats(), (1, 0.5));
    }

    #[tokio::test(start_paused = true)]
    async fn a_ping_in_flight_when_a_main_request_starts_is_cancelled_and_discarded() {
        let mut mock =
            MockProvider::default().with_prefix_cache(PrefixCache::Prefix { ttl: Some(TTL) });
        mock.delay_ms = 20_000;
        let (mock, seen) = mock.with_keepalive(vec![ResponseUsage {
            cache: Some(CacheUsage {
                creation: 0,
                read: 100,
            }),
            ..ResponseUsage::default()
        }]);
        let shared = armed(request_with(AnyProvider::Mock(mock), "hi"));
        let (tx, rx) = watch::channel(MetricsSnapshot::default());
        let mut d = deps(&shared, None);
        d.metrics_tx = Some(tx);
        let Plan::PingAt(_, generation) = shared.plan() else {
            panic!("armed service with a snapshot must plan a ping");
        };

        // `capture` left a wake permit behind; consume it so only the later mark cancels.
        shared.wake.notified().await;
        tokio::join!(d.ping(generation), async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(shared.state(), KeepaliveState::InFlight);
            shared.mark_activity();
        });

        assert_eq!(seen.lock().unwrap().len(), 1, "the ping was started");
        assert_eq!(shared.state(), KeepaliveState::Idle);
        assert_eq!(
            rx.borrow().cache_keepalive_pings,
            0,
            "its result was discarded"
        );
    }

    #[test]
    fn cost_gate_is_open_without_a_cost_tracker() {
        let req = request_with(priced_mock("m"), "hi");
        let shared = Arc::new(Shared::default());
        assert_eq!(deps(&shared, None).gate(&req, 100.0, STANDARD), None);
    }

    #[tokio::test(start_paused = true)]
    async fn ping_below_min_cacheable_is_skipped_and_not_counted() {
        let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-5".into(), 1024);
        let shared = armed(request_with(AnyProvider::Claude(provider), "tiny"));
        let (tx, rx) = watch::channel(MetricsSnapshot::default());
        let mut d = deps(&shared, None);
        d.metrics_tx = Some(tx);
        let Plan::PingAt(_, generation) = shared.plan() else {
            panic!("armed service with a snapshot must plan a ping");
        };

        d.ping(generation).await;

        assert_eq!(shared.state(), KeepaliveState::Idle);
        assert_eq!(rx.borrow().cache_keepalive_pings, 0);
        assert_eq!(shared.inner.lock().consecutive, 0, "a skip is not a ping");
    }

    #[test]
    fn pinged_cache_hit_keeps_the_service_idle_until_the_cap() {
        let shared = armed(request_with(priced_mock("m"), "hi"));
        for expected in [KeepaliveState::Idle, KeepaliveState::Idle] {
            let generation = shared.inner.lock().generation;
            assert!(shared.begin_ping(generation).is_some());
            assert!(shared.finish_ping(generation, 3, hit(0, 100)));
            assert_eq!(shared.state(), expected);
        }
        let generation = shared.inner.lock().generation;
        shared.begin_ping(generation).unwrap();
        shared.finish_ping(generation, 3, hit(0, 100));
        assert_eq!(
            shared.state(),
            KeepaliveState::Stopped(StopReason::CapReached)
        );
    }

    #[test]
    fn result_of_a_ping_that_activity_overtook_is_discarded() {
        let shared = armed(request_with(priced_mock("m"), "hi"));
        let generation = shared.inner.lock().generation;
        shared.begin_ping(generation).unwrap();
        shared.mark_activity();
        assert!(!shared.finish_ping(generation, 6, hit(0, 100)));
        assert_eq!(shared.state(), KeepaliveState::Idle);
    }

    #[test]
    fn failed_pings_back_off_and_count_toward_the_cap() {
        let shared = armed(request_with(priced_mock("m"), "hi"));
        let generation = shared.inner.lock().generation;
        shared.begin_ping(generation).unwrap();
        assert!(shared.finish_ping(generation, 2, PingResult::Failed));
        assert!(shared.inner.lock().retry_at.is_some());
        shared.begin_ping(generation).unwrap();
        shared.finish_ping(generation, 2, PingResult::Failed);
        assert_eq!(
            shared.state(),
            KeepaliveState::Stopped(StopReason::CapReached)
        );
    }

    #[test]
    fn provider_change_clears_cache_stops_but_keeps_user_off() {
        let shared = armed(request_with(priced_mock("m"), "hi"));
        let generation = shared.inner.lock().generation;
        shared.begin_ping(generation).unwrap();
        shared.finish_ping(generation, 6, hit(0, 0));
        assert_eq!(
            shared.state(),
            KeepaliveState::Stopped(StopReason::NoCacheHit)
        );
        shared.provider_changed();
        assert_eq!(shared.state(), KeepaliveState::Idle);
        assert!(shared.inner.lock().snapshot.is_none());

        shared.user_off();
        shared.provider_changed();
        assert_eq!(shared.state(), KeepaliveState::Stopped(StopReason::UserOff));
    }

    #[test]
    fn mismatch_stops_and_clearing_it_resumes() {
        let shared = armed(request_with(priced_mock("m"), "hi"));
        shared.set_mismatch(true);
        assert_eq!(
            shared.state(),
            KeepaliveState::Stopped(StopReason::ProviderMismatch)
        );
        assert!(!shared.is_capturing());
        shared.set_mismatch(false);
        assert_eq!(shared.state(), KeepaliveState::Idle);
    }

    #[test]
    fn user_on_does_not_clear_a_standing_mismatch() {
        let shared = armed(request_with(priced_mock("m"), "hi"));
        shared.set_mismatch(true);
        assert_eq!(
            shared.user_on(),
            KeepaliveState::Stopped(StopReason::ProviderMismatch)
        );
    }

    #[test]
    fn prefix_rewrite_tolerance_boundary() {
        for (creation, stopped) in [(512, false), (513, true)] {
            let shared = armed(request_with(priced_mock("m"), "hi"));
            let generation = shared.inner.lock().generation;
            shared.begin_ping(generation).unwrap();
            shared.finish_ping(generation, 6, hit(creation, 100));
            assert_eq!(
                shared.state() == KeepaliveState::Stopped(StopReason::PrefixRewritten),
                stopped,
                "creation={creation}"
            );
        }
    }

    #[test]
    fn unchanged_tools_share_the_previous_allocation() {
        use zeph_llm::provider::ToolDefinition;

        let tool = ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
            output_schema: None,
        };
        let mut request = request_with(priced_mock("m"), "hi");
        request.tools = Arc::from(vec![tool.clone()]);
        let shared = armed(request);

        let reused = shared.reusable_tools(std::slice::from_ref(&tool)).unwrap();
        let again = shared.reusable_tools(std::slice::from_ref(&tool)).unwrap();
        assert!(Arc::ptr_eq(&reused, &again));

        let mut changed = tool.clone();
        changed.description = "Read a file from disk".into();
        assert!(shared.reusable_tools(&[changed]).is_none());
        assert!(shared.reusable_tools(&[]).is_none());
    }

    #[test]
    fn router_pings_are_gated_with_the_pinged_backend_not_the_last_served_one() {
        use zeph_llm::router::RouterProvider;

        fn named(name: &str, model: &str) -> AnyProvider {
            let mut mock = MockProvider::default()
                .with_name(name)
                .with_prefix_cache(PrefixCache::Prefix { ttl: Some(TTL) });
            mock.model_identifier_override = Some(model.to_owned());
            AnyProvider::Mock(mock)
        }

        // The router reports its first backend ("unpriced", price 0) as its model; the pinged
        // backend "b" is priced so that a read costs more than the write it avoids.
        let router = RouterProvider::new(vec![named("a", "unpriced"), named("b", "priced")]);
        let mut req = request_with(AnyProvider::Router(Box::new(router)), &"x".repeat(40_000));
        req.target = Some(zeph_common::ProviderName::new("b"));
        let tracker = CostTracker::new(true, 0.0).with_pricing("priced", pricing(2.0, 1.0));
        let shared = Arc::new(Shared::default());
        assert_eq!(
            deps(&shared, Some(tracker)).gate(&req, 0.0, STANDARD),
            Some(StopReason::BudgetExhausted)
        );
        assert_eq!(
            req.provider.keepalive_backend(req.target.as_ref()).name(),
            "b"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ping_span_records_the_cache_counters_before_it_closes() {
        use std::sync::Mutex;

        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::{Context, SubscriberExt as _};

        #[derive(Default)]
        struct Fields(Vec<(String, u64)>);
        impl Visit for Fields {
            fn record_u64(&mut self, field: &Field, value: u64) {
                self.0.push((field.name().to_owned(), value));
            }
            fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
        }
        struct Recorder(Arc<Mutex<Vec<(String, u64)>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Recorder {
            fn on_record(
                &self,
                _id: &tracing::span::Id,
                values: &tracing::span::Record<'_>,
                _ctx: Context<'_, S>,
            ) {
                let mut fields = Fields::default();
                values.record(&mut fields);
                self.0.lock().unwrap().extend(fields.0);
            }
        }

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Recorder(Arc::clone(&recorded)));
        let _guard = tracing::subscriber::set_default(subscriber);

        let (mock, _) = MockProvider::default()
            .with_prefix_cache(PrefixCache::Prefix { ttl: Some(TTL) })
            .with_keepalive(vec![ResponseUsage {
                cache: Some(CacheUsage {
                    creation: 7,
                    read: 4_000,
                }),
                ..ResponseUsage::default()
            }]);
        let shared = armed(request_with(AnyProvider::Mock(mock), "hi"));
        let d = deps(&shared, None);
        let Plan::PingAt(_, generation) = shared.plan() else {
            panic!("armed service with a snapshot must plan a ping");
        };
        shared.wake.notified().await;
        d.ping(generation).await;

        let fields = recorded.lock().unwrap().clone();
        assert!(
            fields.contains(&("cache_read".to_owned(), 4_000)),
            "{fields:?}"
        );
        assert!(
            fields.contains(&("cache_creation".to_owned(), 7)),
            "{fields:?}"
        );
    }
}
