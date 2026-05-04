// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Feature-gated metrics helpers for the `MemCoT` distillation pipeline.
//!
//! All public functions are no-ops when the `task-metrics` feature is disabled.
//! This pattern matches the existing `agent_supervisor.rs` convention.

/// Increment the `memcot_distill_total` counter.
#[inline]
pub fn distill_total() {
    #[cfg(feature = "task-metrics")]
    metrics::counter!("memcot_distill_total").increment(1);
}

/// Increment the `memcot_distill_timeout_total` counter.
#[inline]
pub fn distill_timeout() {
    #[cfg(feature = "task-metrics")]
    metrics::counter!("memcot_distill_timeout_total").increment(1);
}

/// Increment the `memcot_distill_error_total` counter.
#[inline]
pub fn distill_error() {
    #[cfg(feature = "task-metrics")]
    metrics::counter!("memcot_distill_error_total").increment(1);
}

/// Increment the `memcot_distill_skipped_total` counter with the given reason label.
///
/// `reason` should be `"interval"` or `"session_cap"`.
#[inline]
pub fn distill_skipped(reason: &'static str) {
    #[cfg(feature = "task-metrics")]
    metrics::counter!("memcot_distill_skipped_total", "reason" => reason).increment(1);
    #[cfg(not(feature = "task-metrics"))]
    let _ = reason;
}
