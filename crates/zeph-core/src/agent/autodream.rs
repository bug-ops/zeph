// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! autoDream background memory consolidation (#2697).
//!
//! Post-session hook: after the agent loop exits, if `min_sessions` and `min_hours` gates
//! both pass, a background consolidation task runs using the configured provider.
//!
//! For MVP, consolidation is performed by calling the existing `run_consolidation_sweep`
//! from `zeph-memory` with the autoDream provider. A dedicated 4-phase LLM subagent is a
//! follow-up improvement tracked in #2697.

use std::time::{Duration, Instant as StdInstant};

use crate::channel::Channel;

/// In-memory autoDream session state.
///
/// Resets on each process restart — only `min_sessions` gate depends on this.
/// `min_hours` gate is enforced via in-process timing using [`Instant`].
pub(crate) struct AutoDreamState {
    /// Number of completed sessions since the last successful consolidation.
    pub(crate) sessions_since_consolidation: u32,
    /// When the last consolidation completed. `None` on first run.
    pub(crate) last_consolidated_at: Option<StdInstant>,
}

impl AutoDreamState {
    pub(crate) const fn new() -> Self {
        Self {
            sessions_since_consolidation: 0,
            last_consolidated_at: None,
        }
    }

    /// Record that a session ended (increments counter).
    pub(super) fn record_session(&mut self) {
        self.sessions_since_consolidation = self.sessions_since_consolidation.saturating_add(1);
    }

    /// Check whether both gates pass for the given config.
    pub(super) fn should_consolidate(&self, min_sessions: u32, min_hours: u32) -> bool {
        if self.sessions_since_consolidation < min_sessions {
            return false;
        }
        if let Some(last) = self.last_consolidated_at {
            let hours_elapsed = last.elapsed().as_secs_f64() / 3600.0;
            if hours_elapsed < f64::from(min_hours) {
                return false;
            }
        }
        // No previous consolidation — always passes hours gate on first run.
        true
    }

    pub(super) fn mark_consolidated(&mut self) {
        self.sessions_since_consolidation = 0;
        self.last_consolidated_at = Some(StdInstant::now());
    }
}

impl<C: Channel> super::Agent<C> {
    /// Run background memory consolidation if autoDream conditions are met.
    ///
    /// Fires after the main agent loop exits. Uses the configured
    /// `consolidation_provider` (falls back to the primary provider).
    /// Respects `max_iterations` as a safety bound via timeout.
    pub(super) async fn maybe_autodream(&mut self) {
        let cfg = self.services.memory.subsystems.autodream_config.clone();
        if !cfg.enabled {
            return;
        }

        self.services.memory.subsystems.autodream.record_session();

        if !self
            .services
            .memory
            .subsystems
            .autodream
            .should_consolidate(cfg.min_sessions, cfg.min_hours)
        {
            tracing::debug!(
                sessions = self
                    .services
                    .memory
                    .subsystems
                    .autodream
                    .sessions_since_consolidation,
                min_sessions = cfg.min_sessions,
                "autoDream: gates not met, skipping"
            );
            return;
        }

        let Some(ref memory) = self.services.memory.persistence.memory else {
            tracing::debug!("autoDream: no memory backend, skipping");
            return;
        };

        tracing::info!("autoDream: starting memory consolidation");
        let _ = self
            .services
            .session
            .status_tx
            .as_ref()
            .map(|tx| tx.send("Consolidating memories…".into()));

        let provider = self.resolve_consolidation_provider(&cfg.consolidation_provider);

        let store = memory.sqlite().clone();
        let consolidation_cfg = zeph_memory::ConsolidationConfig {
            enabled: true,
            sweep_batch_size: 20,
            confidence_threshold: 0.7,
            similarity_threshold: 0.85,
            sweep_interval_secs: 0,
        };

        // Run with a timeout bounded by max_iterations * ~30s per call as a rough limit.
        let timeout = Duration::from_secs(u64::from(cfg.max_iterations) * 30);
        let start = StdInstant::now();

        let result = tokio::time::timeout(
            timeout,
            zeph_memory::run_consolidation_sweep(&store, &provider, &consolidation_cfg),
        )
        .await;

        match result {
            Ok(Ok(sweep_result)) => {
                tracing::info!(
                    merges = sweep_result.merges,
                    updates = sweep_result.updates,
                    skipped = sweep_result.skipped,
                    elapsed_ms = start.elapsed().as_millis(),
                    "autoDream: consolidation complete"
                );
                self.services
                    .memory
                    .subsystems
                    .autodream
                    .mark_consolidated();
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "autoDream: consolidation failed");
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = timeout.as_secs(),
                    "autoDream: consolidation timed out"
                );
            }
        }

        self.flush_taco_hit_counts().await;
    }

    async fn flush_taco_hit_counts(&self) {
        if let Some(ref compressor) = self.services.taco_compressor
            && let Err(e) = compressor.flush_hit_counts().await
        {
            tracing::warn!(error = %e, "autoDream: TACO flush_hit_counts failed");
        }
    }

    /// Resolve the consolidation provider by name, falling back to the primary provider.
    fn resolve_consolidation_provider(&self, name: &str) -> zeph_llm::any::AnyProvider {
        if name.is_empty() {
            return self.provider.clone();
        }
        if let (Some(entry), Some(snapshot)) = (
            self.runtime
                .providers
                .provider_pool
                .iter()
                .find(|e| e.name.as_deref() == Some(name)),
            self.runtime.providers.provider_config_snapshot.as_ref(),
        ) {
            crate::provider_factory::build_provider_for_switch(entry, snapshot).unwrap_or_else(
                |e| {
                    tracing::warn!(
                        provider = name,
                        error = %e,
                        "autoDream: failed to build consolidation_provider, falling back"
                    );
                    self.provider.clone()
                },
            )
        } else {
            self.provider.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autodream_state_gates_sessions() {
        let mut state = AutoDreamState::new();
        // min_sessions=3, min_hours=0 (no hours gate on first run)
        assert!(!state.should_consolidate(3, 0));
        state.record_session();
        assert!(!state.should_consolidate(3, 0));
        state.record_session();
        assert!(!state.should_consolidate(3, 0));
        state.record_session();
        assert!(state.should_consolidate(3, 0));
    }

    #[test]
    fn autodream_state_reset_after_consolidation() {
        let mut state = AutoDreamState::new();
        for _ in 0..5 {
            state.record_session();
        }
        assert!(state.should_consolidate(3, 0));
        state.mark_consolidated();
        // After consolidation: counter resets, min_sessions gate should block again.
        assert!(!state.should_consolidate(3, 0));
    }

    #[test]
    fn autodream_state_hours_gate_passes_on_first_run() {
        let state = AutoDreamState::new();
        // With many sessions, hours gate passes on first run (no prior timestamp).
        let mut s = state;
        for _ in 0..10 {
            s.record_session();
        }
        assert!(s.should_consolidate(3, 24));
    }
}
