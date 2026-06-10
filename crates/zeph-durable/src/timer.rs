// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Durable timers: wakes that survive a process restart.
//!
//! [`DurableContext::sleep_until`](crate::DurableContext::sleep_until) journals a `durable_timers`
//! row at a deterministic [`TimerId`](crate::TimerId) for its program position and parks until the
//! instant arrives. The [`DurableTimerService`] is a background task that polls the backend for due
//! timers, marks them fired, and wakes their parked waiters.
//!
//! # Restart semantics (FR-DE-06)
//!
//! A timer is persisted with its `due_at`, so a process that was down when the instant elapsed
//! recovers correctly: on the first poll after restart the service sees the timer's `due_at` is in
//! the past and fires it immediately. The awaiting execution, replaying to the same `sleep_until`
//! call, re-derives the timer id, finds it already fired, and returns at once instead of sleeping
//! again.

use std::sync::Arc;
use std::time::Duration;

use tracing::Instrument as _;

use crate::backend::DurableBackendEnum;
use crate::backend::local::now_unix_millis;

/// Background task that fires durable timers whose instant has arrived.
///
/// Spawn [`DurableTimerService::run`] on a supervised task. It owns no timer state of its own — the
/// `durable_timers` table is the source of truth — so it is safe to stop and restart: a restarted
/// service re-reads due timers and fires any that elapsed while it was down.
#[derive(Debug)]
pub struct DurableTimerService {
    backend: Arc<DurableBackendEnum>,
    poll_interval: Duration,
}

impl DurableTimerService {
    /// Build the service from the shared backend and a poll cadence.
    ///
    /// `poll_interval` is the worst-case latency between a timer's instant and its firing; the
    /// `promise_poll_interval_secs` config value (default 2 s) is the natural source.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use zeph_durable::{DurableBackendEnum, DurableTimerService, LocalBackend};
    ///
    /// let backend = Arc::new(DurableBackendEnum::Local(Arc::new(
    ///     LocalBackend::open("durable.db", 1_048_576).await?,
    /// )));
    /// let service = DurableTimerService::new(backend, Duration::from_secs(2));
    /// let task = tokio::spawn(service.run());
    /// # let _ = task;
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn new(backend: Arc<DurableBackendEnum>, poll_interval: Duration) -> Self {
        Self {
            backend,
            // Tokio's interval panics on a zero period; clamp to at least 1 ms.
            poll_interval: poll_interval.max(Duration::from_millis(1)),
        }
    }

    /// Run the timer poll loop until the task is aborted.
    ///
    /// Each tick fires every timer whose `due_at` has elapsed (including, on the first tick after a
    /// restart, timers that came due during downtime — FR-DE-06). A backend error is logged and the
    /// loop continues so a transient failure does not strand future timers.
    #[tracing::instrument(name = "durable.timer.run", skip_all)]
    pub async fn run(self) {
        let mut tick = tokio::time::interval(self.poll_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.fire_due()
                .instrument(tracing::info_span!("durable.timer.run.iter"))
                .await;
        }
    }

    /// Fire every currently-due timer once. Exposed for a deterministic test poll.
    #[tracing::instrument(name = "durable.timer.fire_due", skip_all)]
    pub(crate) async fn fire_due(&self) {
        let now = now_unix_millis();
        let due = match self.backend.due_timers(now).await {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!(%error, "durable timer poll failed; will retry next tick");
                return;
            }
        };
        for timer in due {
            match self.backend.mark_timer_fired(timer).await {
                Ok(true) => tracing::debug!(timer_id = %timer.as_uuid(), "durable timer fired"),
                // Already fired by a concurrent poll — nothing to do.
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, timer_id = %timer.as_uuid(), "failed to mark timer fired");
                }
            }
        }
    }
}
