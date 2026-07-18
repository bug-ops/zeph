// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Injectable wall-clock source (#6361).
//!
//! [`ClockSource`] abstracts "what time is it now" so callers on the agent's time-awareness
//! paths (the `get_current_time` tool and periodic time-reminder injection) never call
//! [`std::time::SystemTime::now`] directly — tests substitute [`FixedClock`] instead, keeping
//! them deterministic. Deliberately returns [`std::time::SystemTime`], not a `chrono` type: this
//! crate has no external date/time dependency (see [`crate::timestamp`]).

use std::time::SystemTime;

/// A source of the current wall-clock time.
///
/// Implementors must guarantee `now()` is cheap (no I/O, no blocking) since it is called on
/// hot paths (tool execution, per-turn system-prompt assembly). Callers may assume the
/// returned [`SystemTime`] reflects UTC "now" for [`SystemClock`], or a fixed, caller-chosen
/// instant for [`FixedClock`].
pub trait ClockSource: Send + Sync {
    /// Returns the current time.
    fn now(&self) -> SystemTime;
}

/// Production clock: wraps [`SystemTime::now`].
#[derive(Debug, Clone, Default)]
pub struct SystemClock;

impl ClockSource for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Test/repro clock: always returns the same fixed instant.
///
/// Not gated behind `#[cfg(test)]` — reproducible-run harnesses outside this crate may also
/// want a deterministic clock (mirrors Codex's `clock_source` override).
#[derive(Debug, Clone)]
pub struct FixedClock(pub SystemTime);

impl ClockSource for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_returns_recent_time() {
        let before = SystemTime::now();
        let observed = SystemClock.now();
        let after = SystemTime::now();
        assert!(observed >= before && observed <= after);
    }

    #[test]
    fn fixed_clock_returns_the_same_instant_every_call() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let clock = FixedClock(t);
        assert_eq!(clock.now(), t);
        assert_eq!(clock.now(), t);
    }
}
