// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Metrics helpers for the `MemCoT` distillation pipeline.

/// Increment the `memcot_distill_total` counter.
#[inline]
pub fn distill_total() {
    metrics::counter!("memcot_distill_total").increment(1);
}

/// Increment the `memcot_distill_timeout_total` counter.
#[inline]
pub fn distill_timeout() {
    metrics::counter!("memcot_distill_timeout_total").increment(1);
}

/// Increment the `memcot_distill_error_total` counter.
#[inline]
pub fn distill_error() {
    metrics::counter!("memcot_distill_error_total").increment(1);
}

/// Increment the `memcot_distill_skipped_total` counter with the given reason label.
///
/// `reason` should be `"interval"` or `"session_cap"`.
#[inline]
pub fn distill_skipped(reason: &'static str) {
    metrics::counter!("memcot_distill_skipped_total", "reason" => reason).increment(1);
}
