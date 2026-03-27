// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-task latency ring buffers for classifier calls.
//!
//! [`ClassifierMetrics`] stores the last N latency samples per [`ClassifierTask`]
//! and computes p50/p95 percentiles on demand. Thread-safe via `Mutex`.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use super::ClassifierTask;

/// Default ring buffer capacity per classifier task.
pub const DEFAULT_RING_BUFFER_SIZE: usize = 100;

struct TaskBuffer {
    latencies: VecDeque<Duration>,
    capacity: usize,
    call_count: u64,
}

impl TaskBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            latencies: VecDeque::with_capacity(capacity),
            capacity,
            call_count: 0,
        }
    }

    fn record(&mut self, latency: Duration) {
        if self.latencies.len() == self.capacity {
            self.latencies.pop_front();
        }
        self.latencies.push_back(latency);
        self.call_count += 1;
    }

    /// Compute a percentile using nearest-rank with `.round()` to avoid systematic bias.
    ///
    /// `p` is in 0.0..=1.0. Returns `None` when the buffer is empty.
    fn percentile(&self, p: f64) -> Option<Duration> {
        if self.latencies.is_empty() {
            return None;
        }
        let mut sorted: Vec<Duration> = self.latencies.iter().copied().collect();
        sorted.sort_unstable();
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation
        )]
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        let idx = idx.min(sorted.len() - 1);
        Some(sorted[idx])
    }

    fn snapshot(&self) -> TaskMetricsSnapshot {
        TaskMetricsSnapshot {
            call_count: self.call_count,
            #[allow(clippy::cast_possible_truncation)]
            p50_ms: self.percentile(0.50).map(|d| d.as_millis() as u64),
            #[allow(clippy::cast_possible_truncation)]
            p95_ms: self.percentile(0.95).map(|d| d.as_millis() as u64),
        }
    }
}

/// Read-only snapshot for a single classifier task.
#[derive(Debug, Clone, Default)]
pub struct TaskMetricsSnapshot {
    pub call_count: u64,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
}

/// Read-only snapshot of all classifier metrics.
#[derive(Debug, Clone, Default)]
pub struct ClassifierMetricsSnapshot {
    pub injection: TaskMetricsSnapshot,
    pub pii: TaskMetricsSnapshot,
    pub feedback: TaskMetricsSnapshot,
}

struct ClassifierMetricsInner {
    injection: TaskBuffer,
    pii: TaskBuffer,
    feedback: TaskBuffer,
}

/// Per-task latency ring buffers for classifier calls.
///
/// Thread-safe via `Mutex`. Contention is negligible: classifier calls are
/// infrequent (1–5 per user turn) and each `record()` holds the lock for O(1).
pub struct ClassifierMetrics {
    inner: Mutex<ClassifierMetricsInner>,
}

impl ClassifierMetrics {
    /// Create a new instance with the given ring buffer capacity per task.
    #[must_use]
    pub fn new(ring_buffer_size: usize) -> Self {
        Self {
            inner: Mutex::new(ClassifierMetricsInner {
                injection: TaskBuffer::new(ring_buffer_size),
                pii: TaskBuffer::new(ring_buffer_size),
                feedback: TaskBuffer::new(ring_buffer_size),
            }),
        }
    }

    /// Record a latency sample for `task` and emit a structured tracing event.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (i.e., another thread panicked while holding it).
    pub fn record(&self, task: ClassifierTask, latency: Duration) {
        let snapshot = {
            let mut inner = self.inner.lock().expect("classifier metrics lock poisoned");
            let buf = match task {
                ClassifierTask::Injection => &mut inner.injection,
                ClassifierTask::Pii => &mut inner.pii,
                ClassifierTask::Feedback => &mut inner.feedback,
            };
            buf.record(latency);
            buf.snapshot()
        };

        let task_name = match task {
            ClassifierTask::Injection => "injection",
            ClassifierTask::Pii => "pii",
            ClassifierTask::Feedback => "feedback",
        };

        #[allow(clippy::cast_possible_truncation)]
        let latency_ms_u64 = latency.as_millis() as u64;
        tracing::debug!(
            classifier_task = task_name,
            latency_ms = latency_ms_u64,
            p50_ms = snapshot.p50_ms.unwrap_or(0),
            p95_ms = snapshot.p95_ms.unwrap_or(0),
            call_count = snapshot.call_count,
            "classifier_metrics"
        );
    }

    /// Take a point-in-time snapshot of all metrics for TUI consumption.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (i.e., another thread panicked while holding it).
    #[must_use]
    pub fn snapshot(&self) -> ClassifierMetricsSnapshot {
        let inner = self.inner.lock().expect("classifier metrics lock poisoned");
        ClassifierMetricsSnapshot {
            injection: inner.injection.snapshot(),
            pii: inner.pii.snapshot(),
            feedback: inner.feedback.snapshot(),
        }
    }
}

impl Default for ClassifierMetrics {
    fn default() -> Self {
        Self::new(DEFAULT_RING_BUFFER_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_single_sample_gives_same_p50_p95() {
        let m = ClassifierMetrics::default();
        m.record(ClassifierTask::Injection, Duration::from_millis(42));
        let s = m.snapshot();
        assert_eq!(s.injection.call_count, 1);
        assert_eq!(s.injection.p50_ms, Some(42));
        assert_eq!(s.injection.p95_ms, Some(42));
        assert_eq!(s.pii.call_count, 0);
        assert_eq!(s.pii.p50_ms, None);
        assert_eq!(s.feedback.call_count, 0);
    }

    #[test]
    fn p50_p95_correct_for_ten_samples() {
        let m = ClassifierMetrics::default();
        for i in 1u64..=10 {
            m.record(ClassifierTask::Pii, Duration::from_millis(i * 10));
        }
        let s = m.snapshot();
        assert_eq!(s.pii.call_count, 10);
        // sorted: [10,20,30,40,50,60,70,80,90,100]
        // p50 idx = round(9 * 0.5) = round(4.5) = 5 → sorted[5] = 60
        assert_eq!(s.pii.p50_ms, Some(60));
        // p95 idx = round(9 * 0.95) = round(8.55) = 9 → sorted[9] = 100
        assert_eq!(s.pii.p95_ms, Some(100));
    }

    #[test]
    fn ring_buffer_evicts_oldest_when_full() {
        let m = ClassifierMetrics::new(3);
        m.record(ClassifierTask::Feedback, Duration::from_millis(10));
        m.record(ClassifierTask::Feedback, Duration::from_millis(20));
        m.record(ClassifierTask::Feedback, Duration::from_millis(30));
        // buffer full — next evicts 10ms
        m.record(ClassifierTask::Feedback, Duration::from_millis(40));
        let s = m.snapshot();
        assert_eq!(s.feedback.call_count, 4);
        // sorted ring: [20, 30, 40] — oldest 10ms evicted
        // p50 idx = round(2 * 0.5) = 1 → sorted[1] = 30
        assert_eq!(s.feedback.p50_ms, Some(30));
    }

    #[test]
    fn empty_snapshot_has_none_percentiles() {
        let m = ClassifierMetrics::default();
        let s = m.snapshot();
        assert_eq!(s.injection.p50_ms, None);
        assert_eq!(s.injection.p95_ms, None);
        assert_eq!(s.pii.p50_ms, None);
        assert_eq!(s.feedback.p50_ms, None);
    }

    #[test]
    fn two_samples_p50_returns_higher_with_round() {
        let m = ClassifierMetrics::default();
        m.record(ClassifierTask::Injection, Duration::from_millis(10));
        m.record(ClassifierTask::Injection, Duration::from_millis(20));
        let s = m.snapshot();
        // sorted: [10, 20], p50 idx = round(1 * 0.5) = round(0.5) = 1 → 20ms
        // (nearest-rank rounds 0.5 to 1 with `.round()` — banker's round in Rust: rounds to even,
        // so round(0.5) = 0. p50 idx = 0 → 10ms)
        // Rust `.round()` is IEEE 754 round-half-away-from-zero: 0.5 rounds to 1.0
        assert_eq!(s.injection.p50_ms, Some(20));
    }

    #[test]
    fn p50_p95_correct_for_one_to_ten_ms() {
        let m = ClassifierMetrics::default();
        for i in 1u64..=10 {
            m.record(ClassifierTask::Injection, Duration::from_millis(i));
        }
        let s = m.snapshot();
        assert_eq!(s.injection.call_count, 10);
        // sorted: [1,2,3,4,5,6,7,8,9,10]
        // p50 idx = round(9 * 0.5) = round(4.5) = 5 → sorted[5] = 6ms
        assert_eq!(s.injection.p50_ms, Some(6));
        // p95 idx = round(9 * 0.95) = round(8.55) = 9 → sorted[9] = 10ms
        assert_eq!(s.injection.p95_ms, Some(10));
    }

    #[test]
    fn identical_values_give_same_p50_p95() {
        let m = ClassifierMetrics::new(DEFAULT_RING_BUFFER_SIZE);
        for _ in 0..DEFAULT_RING_BUFFER_SIZE {
            m.record(ClassifierTask::Pii, Duration::from_millis(77));
        }
        let s = m.snapshot();
        assert_eq!(s.pii.call_count, DEFAULT_RING_BUFFER_SIZE as u64);
        assert_eq!(s.pii.p50_ms, Some(77));
        assert_eq!(s.pii.p95_ms, Some(77));
    }

    #[test]
    fn ring_buffer_evicts_oldest_at_default_capacity() {
        let m = ClassifierMetrics::new(DEFAULT_RING_BUFFER_SIZE);
        // Fill buffer: samples 1..=100ms
        for i in 1u64..=DEFAULT_RING_BUFFER_SIZE as u64 {
            m.record(ClassifierTask::Injection, Duration::from_millis(i));
        }
        // Record one more — evicts the oldest (1ms)
        m.record(ClassifierTask::Injection, Duration::from_millis(200));
        let s = m.snapshot();
        assert_eq!(s.injection.call_count, DEFAULT_RING_BUFFER_SIZE as u64 + 1);
        // Buffer now holds [2,3,...,100,200] — 100 entries, min is 2ms
        // p50 idx = round(99 * 0.5) = round(49.5) = 50 → sorted[50] = 52ms
        assert_eq!(s.injection.p50_ms, Some(52));
        // p95 idx = round(99 * 0.95) = round(94.05) = 94 → sorted[94] = 96ms
        assert_eq!(s.injection.p95_ms, Some(96));
    }
}
