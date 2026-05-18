// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::atomic::{AtomicU64, Ordering};

/// Prometheus-compatible counters for the five-signal retrieval subsystem.
///
/// All counters use `AtomicU64` for lock-free access. When the `prometheus` feature
/// is enabled, the values are exported via the existing `zeph-core` metrics registry.
/// In TUI mode the values are readable from the metrics panel.
#[derive(Debug, Default)]
pub struct FiveSignalMetrics {
    /// Total five-signal recall invocations (`five_signal_recall_total`).
    pub recall_total: AtomicU64,
    /// Total consolidation daemon runs (`consolidation_daemon_runs_total`).
    pub consolidation_runs_total: AtomicU64,
    /// Total facts promoted to Qdrant by the daemon (`consolidation_promoted_total`).
    pub promoted_total: AtomicU64,
    /// Total facts demoted to `episodic_only` by the daemon (`consolidation_demoted_total`).
    pub demoted_total: AtomicU64,
}

impl FiveSignalMetrics {
    /// Increment the recall counter by 1.
    #[inline]
    pub fn inc_recall(&self) {
        self.recall_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the consolidation daemon run counter by 1.
    #[inline]
    pub fn inc_consolidation_run(&self) {
        self.consolidation_runs_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the promoted facts counter by `n`.
    #[inline]
    pub fn add_promoted(&self, n: u64) {
        self.promoted_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Increment the demoted facts counter by `n`.
    #[inline]
    pub fn add_demoted(&self, n: u64) {
        self.demoted_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Snapshot of current recall count.
    #[must_use]
    #[inline]
    pub fn recall_count(&self) -> u64 {
        self.recall_total.load(Ordering::Relaxed)
    }

    /// Snapshot of current promoted count.
    #[must_use]
    #[inline]
    pub fn promoted_count(&self) -> u64 {
        self.promoted_total.load(Ordering::Relaxed)
    }

    /// Snapshot of current demoted count.
    #[must_use]
    #[inline]
    pub fn demoted_count(&self) -> u64 {
        self.demoted_total.load(Ordering::Relaxed)
    }
}
