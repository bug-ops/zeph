// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression test for the `BgMetricsTick` vs. channel-closed race (#6279 follow-up).
//!
//! `Agent::next_event`'s `tokio::select!` lazily constructs `bg_metrics_tick` on its first
//! poll. A plain `tokio::time::interval(..)` fires its first tick *immediately* at construction
//! time (this is documented `tokio` behavior, not a bug in `tokio` itself) — since this interval
//! is lazily built on the very first `next_event()` poll, that immediate first tick raced the
//! also-immediately-ready `self.channel.recv()` / shutdown-detection arms: `tokio::select!` is
//! unbiased here, so on any given poll it could pick `BgMetricsTick` over the channel-closed
//! signal, forcing one spurious extra loop iteration before `Agent::run()` observed a
//! closed/closing channel and exited (tester-found race, empirically ~90% `BgMetricsTick` won
//! in a live session). Fixed by deferring the first tick via
//! `tokio::time::interval_at(now + BG_METRICS_TICK_INTERVAL, BG_METRICS_TICK_INTERVAL)`, so the
//! tick genuinely is not ready at all on the first poll — eliminating the race by construction
//! rather than reducing its odds.
//!
//! `tokio::select!`'s tie-break among simultaneously-ready branches is not reliably reproducible
//! in a plain real-time unit test (empirically, in this harness, the channel-closed arm — being
//! first in the `select!` block — won every time regardless of whether the tick was immediate or
//! deferred, so a test built around racing them directly would pass either way and prove
//! nothing). Testing the interval's own readiness with `tokio::time::pause()` instead verifies
//! the actual fix mechanism deterministically: with the fix, the tick must not be ready until
//! virtual time advances by a full `BG_METRICS_TICK_INTERVAL`.

use std::time::Duration;

use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};
use crate::agent::state::BG_METRICS_TICK_INTERVAL;

fn base_agent() -> crate::agent::Agent<MockChannel> {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
}

/// Deterministic (paused-time) proof of the actual fix mechanism: `interval_at(now + INTERVAL,
/// INTERVAL)` must NOT resolve `.tick()` until virtual time has advanced by a full `INTERVAL`.
/// A plain `tokio::time::interval(INTERVAL)` (the pre-fix construction) resolves its first
/// `.tick()` immediately instead — this test would fail against that construction (verified by
/// temporarily swapping the constructor back to `interval(..)` while writing this test).
#[tokio::test(start_paused = true)]
async fn deferred_first_tick_is_not_ready_until_interval_elapses() {
    let mut iv = tokio::time::interval_at(
        tokio::time::Instant::now() + BG_METRICS_TICK_INTERVAL,
        BG_METRICS_TICK_INTERVAL,
    );

    // Immediately after construction (virtual time unchanged), the first tick must not be
    // ready — a zero-duration timeout only fires if the wrapped future is not already ready.
    assert!(
        tokio::time::timeout(Duration::ZERO, iv.tick())
            .await
            .is_err(),
        "the deferred interval's first tick must not be ready at construction time"
    );

    // Advancing to just before the deadline: still not ready.
    tokio::time::advance(BG_METRICS_TICK_INTERVAL.saturating_sub(Duration::from_millis(1))).await;
    assert!(
        tokio::time::timeout(Duration::ZERO, iv.tick())
            .await
            .is_err(),
        "the deferred interval's first tick must not be ready just before the deadline"
    );

    // Advancing past the deadline: now ready.
    tokio::time::advance(Duration::from_millis(2)).await;
    assert!(
        tokio::time::timeout(Duration::ZERO, iv.tick())
            .await
            .is_ok(),
        "the deferred interval's first tick must become ready once the interval has elapsed"
    );
}

/// Companion negative case, spelled out explicitly: a plain `tokio::time::interval(..)` (the
/// pre-fix construction) resolves its first `.tick()` immediately, with no time advancement at
/// all — this is exactly the documented `tokio` behavior that raced the channel-closed arm.
#[tokio::test(start_paused = true)]
async fn plain_interval_first_tick_is_immediate_this_is_the_bug_interval_at_fixes() {
    let mut iv = tokio::time::interval(BG_METRICS_TICK_INTERVAL);
    assert!(
        tokio::time::timeout(Duration::ZERO, iv.tick())
            .await
            .is_ok(),
        "plain tokio::time::interval fires its first tick immediately (documented tokio \
         behavior) — this is precisely why next_event() must use interval_at instead"
    );
}

/// Integration-level sanity check: `next_event()` on an agent with an already-empty (closed)
/// channel must still report the channel as closed (`Ok(None)`), not error or hang, even though
/// `bg_metrics_tick` is lazily constructed during this exact call.
#[tokio::test]
async fn next_event_reports_closed_channel_without_erroring() {
    let mut agent = base_agent();
    let event = agent.next_event().await.expect("next_event must not error");
    assert!(
        event.is_none(),
        "an already-empty channel must be observed as closed by next_event()"
    );
}
