// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Idle-reaping and shutdown methods for `ZephAcpAgentState`.
//!
//! Groups the background idle-session reaper, agent shutdown, and LRU eviction-on-full
//! logic so session-lifecycle capacity management is isolated from dispatch logic in
//! [`super`].

use std::sync::Arc;
use std::sync::atomic::Ordering;

use agent_client_protocol as acp;
use zeph_common::task_supervisor::{RestartPolicy, TaskDescriptor};

use super::ZephAcpAgentState;

impl ZephAcpAgentState {
    /// Spawn a background task that periodically evicts idle sessions.
    ///
    /// The task runs until the agent's `reaper_cancel` token is cancelled.
    /// Registered in `task_supervisor` for lifecycle observability.
    ///
    /// Note: sessions evicted by the idle reaper are forcibly removed without sending a
    /// cumulative usage summary. Only graceful `do_close_session` emits a final `UsageUpdate`.
    pub fn start_idle_reaper(&self) {
        let sessions = Arc::clone(&self.sessions);
        let idle_timeout = self.idle_timeout;
        let cancel = self.reaper_cancel.clone();
        self.task_supervisor.spawn(TaskDescriptor {
            name: "acp_idle_reaper",
            restart: RestartPolicy::Restart {
                max: 0,
                base_delay: std::time::Duration::from_secs(1),
            },
            factory: move || {
                let sessions = Arc::clone(&sessions);
                let cancel = cancel.clone();
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_mins(1));
                    interval.tick().await; // skip first tick
                    loop {
                        tokio::select! {
                            biased;
                            () = cancel.cancelled() => break,
                            _ = interval.tick() => {}
                        }
                        let now_ms = u64::try_from(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis(),
                        )
                        .unwrap_or(u64::MAX);
                        let idle_timeout_ms =
                            u64::try_from(idle_timeout.as_millis()).unwrap_or(u64::MAX);
                        let expired: Vec<acp::schema::v1::SessionId> = sessions
                            .lock()
                            .iter()
                            .filter(|(_, e)| {
                                let idle_ms =
                                    now_ms.saturating_sub(e.last_active_ms.load(Ordering::Relaxed));
                                e.output_rx.lock().is_some() && idle_ms > idle_timeout_ms
                            })
                            .map(|(id, _)| id.clone())
                            .collect();
                        for id in expired {
                            if let Some(entry) = sessions.lock().remove(&id) {
                                entry.cancel_signal.notify_one();
                                tracing::debug!(
                                    session_id = %id,
                                    "evicted idle ACP session (timeout)"
                                );
                            }
                        }
                    }
                }
            },
        });
    }

    /// Cancel the idle reaper task.
    pub fn shutdown(&self) {
        self.reaper_cancel.cancel();
    }
    /// Evict the oldest idle session when the session limit is reached.
    ///
    /// Idle is defined as: `output_rx` is `Some` (no prompt in flight).
    /// The two separate `self.sessions.lock()` calls below are intentional: the first is
    /// scoped to just the `.iter()` scan + `min_by_key` that picks the victim, so the lock
    /// isn't held for that O(n) work any longer than necessary; the second (re-)acquires it to
    /// remove the victim. That second guard, via `if let`'s temporary lifetime extension, stays
    /// held for the rest of the block — including `entry.cancel_signal.notify_one()` and
    /// `entry`'s `Drop` (which now also aborts its agent-loop `JoinHandle`, #6674) — which is
    /// safe only because nothing on either path touches `self.sessions` again.
    pub(crate) fn evict_oldest_idle_session_if_full(&self) -> acp::Result<()> {
        if self.sessions.lock().len() < self.max_sessions {
            return Ok(());
        }
        let evict_id = {
            let sessions = self.sessions.lock();
            sessions
                .iter()
                .filter(|(_, e)| e.output_rx.lock().is_some())
                .min_by_key(|(_, e)| e.last_active_ms.load(Ordering::Relaxed))
                .map(|(id, _)| id.clone())
        };
        match evict_id {
            Some(id) => {
                if let Some(entry) = self.sessions.lock().remove(&id) {
                    entry.cancel_signal.notify_one();
                    tracing::debug!(session_id = %id, "evicted idle ACP session (LRU)");
                }
                Ok(())
            }
            None => Err(acp::Error::internal_error().data("session limit reached")),
        }
    }
}
