// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::provider::ResponseUsage;

/// Holds the usage snapshot and time-to-first-byte of a provider's last response.
///
/// The snapshot is one value behind one lock, so a reader never observes counters of two
/// different responses. Providers build the whole [`ResponseUsage`] locally and publish it
/// with [`record`](Self::record); a field a response did not report is `None` rather than
/// carried over from an earlier response.
///
/// Usage belongs to the instance that made the call: [`Clone`] yields a fresh, empty tracker
/// instead of crediting the clone with a call it never made.
#[derive(Debug, Default)]
pub(crate) struct UsageTracker {
    last: parking_lot::Mutex<Option<ResponseUsage>>,
    /// Time-to-first-byte of the last HTTP round-trip, in milliseconds (issue #6549).
    ///
    /// Recorded by `retry::send_with_retry` (or the equivalent per-attempt retry loop for
    /// providers that don't use that shared helper, e.g. gonka) around each attempt's
    /// response-headers arrival — a TTFB proxy, since that is the earliest point observable
    /// from inside `zeph-llm` on the dominant non-streaming path. `None` for providers with
    /// no HTTP round-trip (Candle). This is always a proxy value, never true first-token
    /// time: the one production streaming path (speculative decoding) captures true TTFT
    /// separately, at its own stream-consumption point in `zeph-core`
    /// (`agent::speculative::stream_drainer::SpeculativeStreamDrainer::drive`), and that
    /// value takes priority over this field's TTFB when building a `usage_records` row —
    /// see `Agent::build_usage_record`'s `stream_ttft_ms` parameter.
    last_ttft: parking_lot::Mutex<Option<u64>>,
}

impl UsageTracker {
    /// Forget the usage of the previous response, keeping only the time-to-first-byte.
    ///
    /// Called when a request opens, so a call that fails before reporting any usage leaves no
    /// values from an earlier call behind. The TTFB survives because the HTTP layer records it
    /// for the request that is about to be sent.
    pub(crate) fn clear_response(&self) {
        self.record(None);
    }

    /// Publish the usage of the most recent response; `None` clears the previous snapshot.
    pub(crate) fn record(&self, usage: Option<ResponseUsage>) {
        *self.last.lock() = usage;
    }

    /// Return the snapshot published for the most recent response.
    pub(crate) fn last(&self) -> Option<ResponseUsage> {
        *self.last.lock()
    }

    /// Record the time-to-first-token/byte (milliseconds) of the most recent HTTP attempt.
    pub(crate) fn record_ttft(&self, ms: u64) {
        *self.last_ttft.lock() = Some(ms);
    }

    /// Returns the time-to-first-token/byte (milliseconds) of the last HTTP round-trip.
    pub(crate) fn last_ttft_ms(&self) -> Option<u64> {
        *self.last_ttft.lock()
    }
}

impl Clone for UsageTracker {
    /// Returns a fresh, empty tracker: usage belongs to the instance that made the call.
    fn clone(&self) -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{AuxiliaryUsage, CacheUsage};

    #[test]
    fn last_ttft_ms_initially_none() {
        let tracker = UsageTracker::default();
        assert_eq!(tracker.last_ttft_ms(), None);
    }

    #[test]
    fn record_ttft_updates_last_ttft_ms() {
        let tracker = UsageTracker::default();
        tracker.record_ttft(42);
        assert_eq!(tracker.last_ttft_ms(), Some(42));
    }

    #[test]
    fn record_ttft_overwrites_previous_value() {
        let tracker = UsageTracker::default();
        tracker.record_ttft(100);
        tracker.record_ttft(7);
        assert_eq!(
            tracker.last_ttft_ms(),
            Some(7),
            "issue #6549 D-S2: a later attempt's TTFB must overwrite an earlier one"
        );
    }

    #[test]
    fn record_replaces_whole_snapshot() {
        let tracker = UsageTracker::default();
        tracker.record(Some(ResponseUsage {
            input: 1,
            output: 2,
            cache: Some(CacheUsage {
                creation: 3,
                read: 4,
            }),
            reasoning: Some(5),
            auxiliary: Some(AuxiliaryUsage::default()),
        }));
        tracker.record(Some(ResponseUsage {
            input: 10,
            output: 20,
            ..ResponseUsage::default()
        }));
        let last = tracker.last().expect("snapshot recorded");
        assert_eq!((last.input, last.output), (10, 20));
        assert_eq!(last.cache, None, "cache must not carry over");
        assert_eq!(last.reasoning, None, "reasoning must not carry over");
        assert_eq!(last.auxiliary, None);
    }

    #[test]
    fn clear_response_keeps_only_ttft() {
        let tracker = UsageTracker::default();
        tracker.record(Some(ResponseUsage::default()));
        tracker.record_ttft(9);
        tracker.clear_response();
        assert_eq!(tracker.last(), None);
        assert_eq!(tracker.last_ttft_ms(), Some(9));
    }

    #[test]
    fn clone_resets_snapshot_and_ttft() {
        let tracker = UsageTracker::default();
        tracker.record(Some(ResponseUsage::default()));
        tracker.record_ttft(42);
        let cloned = tracker.clone();
        assert_eq!(cloned.last(), None);
        assert_eq!(
            cloned.last_ttft_ms(),
            None,
            "UsageTracker::clone must reset all state, including ttft"
        );
    }
}
