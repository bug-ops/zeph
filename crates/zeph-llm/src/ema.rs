// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-provider EMA tracker for latency-aware [`super::router::RouterProvider`] ordering.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

/// Per-provider EMA statistics used for routing decisions.
#[derive(Debug, Clone)]
pub struct ProviderStats {
    pub success_ema: f64,
    pub latency_ema_ms: f64,
    pub total_calls: u64,
}

impl Default for ProviderStats {
    fn default() -> Self {
        Self {
            success_ema: 1.0,      // optimistic prior
            latency_ema_ms: 500.0, // neutral prior
            total_calls: 0,
        }
    }
}

/// Thread-safe EMA tracker for multiple named providers.
#[derive(Debug, Clone)]
pub struct EmaTracker {
    stats: Arc<Mutex<HashMap<String, ProviderStats>>>,
    alpha: f64,
    reorder_interval: u64,
    call_counter: Arc<Mutex<u64>>,
}

impl EmaTracker {
    #[must_use]
    pub fn new(alpha: f64, reorder_interval: u64) -> Self {
        Self {
            stats: Arc::new(Mutex::new(HashMap::new())),
            alpha,
            reorder_interval,
            call_counter: Arc::new(Mutex::new(0)),
        }
    }

    /// Record the outcome of a provider call.
    pub fn record(&self, provider_name: &str, success: bool, latency_ms: u64) {
        let mut stats = self.stats.lock();
        let entry = stats.entry(provider_name.to_owned()).or_default();
        let success_val = if success { 1.0 } else { 0.0 };
        entry.success_ema = self.alpha * success_val + (1.0 - self.alpha) * entry.success_ema;
        #[allow(clippy::cast_precision_loss)]
        let latency_f = latency_ms as f64;
        entry.latency_ema_ms = self.alpha * latency_f + (1.0 - self.alpha) * entry.latency_ema_ms;
        entry.total_calls += 1;
    }

    /// If the reorder interval has been reached, return the recommended provider order.
    ///
    /// Returns `None` if the interval has not been reached yet.
    #[must_use]
    pub fn maybe_reorder(&self, current_order: &[String]) -> Option<Vec<String>> {
        let mut counter = self.call_counter.lock();
        *counter += 1;
        if self.reorder_interval == 0 || !(*counter).is_multiple_of(self.reorder_interval) {
            return None;
        }

        let stats = self.stats.lock();
        let mut scored: Vec<(String, f64)> = current_order
            .iter()
            .map(|name| {
                let s = stats.get(name).cloned().unwrap_or_default();
                // Higher score = preferred. Penalize high latency (normalized to ~0-1).
                let score = s.success_ema - s.latency_ema_ms / 10_000.0;
                (name.clone(), score)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Some(scored.into_iter().map(|(name, _)| name).collect())
    }

    /// Return a snapshot of current stats for all tracked providers.
    #[must_use]
    pub fn snapshot(&self) -> HashMap<String, ProviderStats> {
        self.stats.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stats_optimistic_prior() {
        let s = ProviderStats::default();
        assert!((s.success_ema - 1.0).abs() < f64::EPSILON);
        assert!(s.latency_ema_ms > 0.0);
        assert_eq!(s.total_calls, 0);
    }

    #[test]
    fn new_tracker_empty_stats() {
        let t = EmaTracker::new(0.3, 10);
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn record_updates_success_ema() {
        let t = EmaTracker::new(0.5, 100);
        t.record("p1", true, 100);
        let snap = t.snapshot();
        let s = snap.get("p1").unwrap();
        // alpha=0.5: 0.5*1.0 + 0.5*1.0 = 1.0 (starts at prior 1.0, first success)
        assert!((s.success_ema - 1.0).abs() < 1e-9);
        t.record("p1", false, 100);
        let snap = t.snapshot();
        let s = snap.get("p1").unwrap();
        // 0.5*0.0 + 0.5*1.0 = 0.5
        assert!((s.success_ema - 0.5).abs() < 1e-9);
    }

    #[test]
    fn record_updates_latency_ema() {
        let t = EmaTracker::new(0.5, 100);
        t.record("p1", true, 200);
        let snap = t.snapshot();
        let s = snap.get("p1").unwrap();
        // 0.5*200 + 0.5*500 = 350
        assert!((s.latency_ema_ms - 350.0).abs() < 1e-6);
    }

    #[test]
    fn record_increments_total_calls() {
        let t = EmaTracker::new(0.3, 100);
        t.record("p1", true, 10);
        t.record("p1", true, 10);
        assert_eq!(t.snapshot().get("p1").unwrap().total_calls, 2);
    }

    #[test]
    fn maybe_reorder_returns_none_before_interval() {
        let t = EmaTracker::new(0.3, 10);
        let order = vec!["p1".to_string(), "p2".to_string()];
        for _ in 0..9 {
            assert!(t.maybe_reorder(&order).is_none());
        }
    }

    #[test]
    fn maybe_reorder_returns_order_at_interval() {
        let t = EmaTracker::new(0.3, 10);
        let order = vec!["p1".to_string(), "p2".to_string()];
        for _ in 0..9 {
            let _ = t.maybe_reorder(&order);
        }
        let result = t.maybe_reorder(&order);
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 2);
    }

    #[test]
    fn maybe_reorder_fast_reliable_rises_to_top() {
        let t = EmaTracker::new(1.0, 1); // alpha=1 → immediate update, interval=1
        // p2: success, low latency → higher score
        t.record("p1", false, 9000);
        let _ = t.maybe_reorder(&["p1".to_string(), "p2".to_string()]);
        t.record("p2", true, 10);
        let result = t
            .maybe_reorder(&["p1".to_string(), "p2".to_string()])
            .unwrap();
        assert_eq!(result[0], "p2");
    }

    #[test]
    fn maybe_reorder_slow_unreliable_drops() {
        let t = EmaTracker::new(1.0, 1);
        // p1: slow + unreliable
        t.record("p1", false, 9000);
        let _ = t.maybe_reorder(&["p1".to_string(), "p2".to_string()]);
        // p2: fast + reliable
        t.record("p2", true, 10);
        let result = t
            .maybe_reorder(&["p1".to_string(), "p2".to_string()])
            .unwrap();
        assert_eq!(result[result.len() - 1], "p1");
    }

    #[test]
    fn maybe_reorder_interval_zero_always_none() {
        let t = EmaTracker::new(0.3, 0);
        let order = vec!["p1".to_string()];
        for _ in 0..100 {
            assert!(
                t.maybe_reorder(&order).is_none(),
                "interval=0 should never trigger reorder"
            );
        }
    }

    #[test]
    fn record_multiple_providers_independent() {
        let t = EmaTracker::new(0.5, 100);
        t.record("p1", true, 100);
        t.record("p2", false, 200);

        let snap = t.snapshot();
        let p1 = snap.get("p1").unwrap();
        let p2 = snap.get("p2").unwrap();

        assert!(
            p1.success_ema > p2.success_ema,
            "p1 success should be higher than p2"
        );
        assert_eq!(p1.total_calls, 1);
        assert_eq!(p2.total_calls, 1);
    }

    #[test]
    fn maybe_reorder_empty_order_returns_empty() {
        let t = EmaTracker::new(0.3, 1);
        // interval=1 → triggers on first call
        let result = t.maybe_reorder(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn record_many_failures_drives_success_ema_toward_zero() {
        let t = EmaTracker::new(0.5, 100);
        // After many failures with alpha=0.5, EMA converges toward 0.0
        for _ in 0..20 {
            t.record("p1", false, 100);
        }
        let snap = t.snapshot();
        let s = snap.get("p1").unwrap();
        assert!(
            s.success_ema < 0.01,
            "success EMA should be near 0 after many failures, got {}",
            s.success_ema
        );
    }
}
