// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Process-lifetime monotonic millisecond clock shared across crates.
//!
//! Unlike a wall-clock timestamp (`SystemTime`), this origin is immune to NTP steps and
//! manual clock adjustments — a backward time jump between a writer and a reader can never
//! produce a spurious elapsed-time reading. Intended for in-process liveness/heartbeat
//! signals (e.g. idle-timeout detection) where writer and reader always run in the same
//! process and only relative deltas matter, never the absolute value.

use std::sync::LazyLock;
use std::time::Instant;

static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Milliseconds elapsed since an arbitrary process-lifetime origin (the first call to any
/// function in this module).
///
/// Monotonic and immune to wall-clock adjustments. Compare two readings with
/// `saturating_sub` to compute an elapsed duration; the absolute value has no meaning on
/// its own.
///
/// # Examples
///
/// ```rust
/// let t0 = zeph_common::monotonic_millis();
/// std::thread::sleep(std::time::Duration::from_millis(5));
/// let t1 = zeph_common::monotonic_millis();
/// assert!(t1 >= t0);
/// assert!(t1.saturating_sub(t0) >= 5);
/// ```
#[must_use]
pub fn monotonic_millis() -> u64 {
    u64::try_from(PROCESS_START.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_millis_is_non_decreasing() {
        let a = monotonic_millis();
        let b = monotonic_millis();
        assert!(b >= a);
    }

    #[test]
    fn monotonic_millis_advances_with_real_sleep() {
        let a = monotonic_millis();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let b = monotonic_millis();
        assert!(b.saturating_sub(a) >= 10);
    }
}
