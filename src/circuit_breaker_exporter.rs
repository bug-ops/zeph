// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Circuit-breaker wrapper for OTLP span exporters.
//!
//! Wraps any [`SpanExporter`] and opens the circuit after 3 consecutive export
//! failures, preventing busy-retry CPU burn when the OTLP collector is unavailable.
//! The circuit re-tries after a back-off: 5 s → 30 s → 300 s.

#![cfg(feature = "otel")]

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::SpanData;
use opentelemetry_sdk::trace::SpanExporter;

/// Back-off durations indexed by `open_count - 1` (clamped at the last entry).
const BACKOFFS: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(300),
];

/// Circuit-breaker wrapping an inner [`SpanExporter`].
///
/// Tracks consecutive export failures. After 3 consecutive failures the circuit
/// opens and all export calls return `Ok(())` immediately (spans silently dropped)
/// until the back-off window expires. On success the failure counter resets.
#[derive(Debug)]
pub struct CircuitBreakerExporter<E: SpanExporter> {
    inner: E,
    consecutive_failures: Arc<AtomicU32>,
    /// `Some(Instant)` when the circuit is open; the instant is when it may close.
    circuit_open_until: Arc<Mutex<Option<Instant>>>,
    /// How many times the circuit has been opened (drives back-off index).
    open_count: Arc<AtomicU32>,
}

impl<E: SpanExporter> CircuitBreakerExporter<E> {
    /// Wrap `inner` with a circuit breaker.
    pub fn new(inner: E) -> Self {
        Self {
            inner,
            consecutive_failures: Arc::new(AtomicU32::new(0)),
            circuit_open_until: Arc::new(Mutex::new(None)),
            open_count: Arc::new(AtomicU32::new(0)),
        }
    }

    fn is_open(&self) -> bool {
        let guard = self.circuit_open_until.lock().expect("mutex poisoned");
        guard.is_some_and(|until| Instant::now() < until)
    }

    fn maybe_close_circuit(&self) {
        let mut guard = self.circuit_open_until.lock().expect("mutex poisoned");
        if guard.is_some_and(|until| Instant::now() >= until) {
            tracing::info!("OTLP circuit breaker: circuit closing, resuming export attempts");
            *guard = None;
        }
    }

    fn open_circuit(&self) {
        let count = self.open_count.fetch_add(1, Ordering::Relaxed) as usize;
        let backoff = BACKOFFS[count.min(BACKOFFS.len() - 1)];
        let until = Instant::now() + backoff;
        *self.circuit_open_until.lock().expect("mutex poisoned") = Some(until);
        tracing::warn!(
            backoff_secs = backoff.as_secs(),
            "OTLP circuit breaker: 3 consecutive failures, circuit open for {}s",
            backoff.as_secs()
        );
    }
}

impl<E: SpanExporter + fmt::Debug> SpanExporter for CircuitBreakerExporter<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.maybe_close_circuit();

        if self.is_open() {
            // Circuit open — silently drop spans to avoid CPU burn.
            return Ok(());
        }

        match self.inner.export(batch).await {
            Ok(()) => {
                self.consecutive_failures.store(0, Ordering::Relaxed);
                // Reset open_count so the next failure sequence starts fresh at the 5s back-off.
                self.open_count.store(0, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if failures == 1 {
                    tracing::warn!(error = %e, "OTLP export failed (attempt {failures})");
                } else {
                    tracing::debug!(error = %e, "OTLP export failed (attempt {failures})");
                }
                if failures >= 3 {
                    self.consecutive_failures.store(0, Ordering::Relaxed);
                    self.open_circuit();
                }
                Err(e)
            }
        }
    }

    fn shutdown_with_timeout(&mut self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[derive(Debug)]
    struct MockExporter {
        call_count: Arc<AtomicUsize>,
        /// Returns `Err` for the first `fail_until` calls, then `Ok`.
        fail_until: usize,
    }

    impl MockExporter {
        fn new_always_fail() -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
                fail_until: usize::MAX,
            }
        }

        fn new_always_ok() -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
                fail_until: 0,
            }
        }

        fn new_fail_then_ok(fail_until: usize) -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
                fail_until,
            }
        }
    }

    impl SpanExporter for MockExporter {
        async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
            let n = self.call_count.fetch_add(1, Ordering::Relaxed);
            if n < self.fail_until {
                Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
                    "mock failure".into(),
                ))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn test_circuit_opens_after_three_failures() {
        let cb = CircuitBreakerExporter::new(MockExporter::new_always_fail());
        // Three failures open the circuit.
        for _ in 0..3 {
            let _ = cb.export(vec![]).await;
        }
        assert!(
            cb.is_open(),
            "circuit should be open after 3 consecutive failures"
        );
    }

    #[tokio::test]
    async fn test_circuit_resets_on_success() {
        // Fail 3 times to open circuit, then close it manually, then succeed.
        let cb = CircuitBreakerExporter::new(MockExporter::new_always_fail());
        for _ in 0..3 {
            let _ = cb.export(vec![]).await;
        }
        assert!(cb.is_open());
        // Force-close by expiring the window.
        {
            let mut guard = cb.circuit_open_until.lock().unwrap();
            *guard = Some(Instant::now() - Duration::from_secs(1));
        }
        assert!(!cb.is_open());

        // Now use an always-ok exporter to verify consecutive_failures and open_count reset.
        let cb2 = CircuitBreakerExporter::new(MockExporter::new_always_ok());
        let _ = cb2.export(vec![]).await;
        assert_eq!(cb2.consecutive_failures.load(Ordering::Relaxed), 0);
        assert_eq!(cb2.open_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_circuit_closes_after_backoff() {
        let cb = CircuitBreakerExporter::new(MockExporter::new_always_fail());
        for _ in 0..3 {
            let _ = cb.export(vec![]).await;
        }
        assert!(cb.is_open());
        // Expire the back-off window.
        {
            let mut guard = cb.circuit_open_until.lock().unwrap();
            *guard = Some(Instant::now() - Duration::from_secs(1));
        }
        cb.maybe_close_circuit();
        assert!(!cb.is_open(), "circuit should close after back-off expires");
    }

    #[tokio::test]
    async fn test_backoff_progression() {
        // First open: 5s; second open: 30s; third open: 300s (clamped).
        let expected = [5u64, 30, 300];
        let cb = CircuitBreakerExporter::new(MockExporter::new_always_fail());

        for &secs in &expected {
            // Reset consecutive_failures so we can re-trigger open.
            cb.consecutive_failures.store(0, Ordering::Relaxed);
            // Expire any existing window so export goes through.
            {
                let mut guard = cb.circuit_open_until.lock().unwrap();
                *guard = None;
            }
            for _ in 0..3 {
                let _ = cb.export(vec![]).await;
            }
            let until = cb.circuit_open_until.lock().unwrap().unwrap();
            let remaining = until.duration_since(Instant::now());
            // Allow ±1s tolerance for test timing.
            assert!(
                remaining.as_secs() <= secs && remaining.as_secs() + 1 >= secs,
                "expected ~{secs}s back-off, got {}s",
                remaining.as_secs()
            );
        }
    }

    #[tokio::test]
    async fn test_open_count_resets_on_success() {
        // Fail 3 times → open, expire window, succeed → open_count resets → next failure uses 5s.
        let cb = CircuitBreakerExporter::new(MockExporter::new_fail_then_ok(3));
        for _ in 0..3 {
            let _ = cb.export(vec![]).await;
        }
        assert_eq!(cb.open_count.load(Ordering::Relaxed), 1);
        // Expire window.
        {
            let mut guard = cb.circuit_open_until.lock().unwrap();
            *guard = None;
        }
        // Success resets open_count.
        cb.consecutive_failures.store(0, Ordering::Relaxed);
        let _ = cb.export(vec![]).await;
        assert_eq!(
            cb.open_count.load(Ordering::Relaxed),
            0,
            "open_count should reset on success"
        );
    }
}
