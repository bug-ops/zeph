// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Tracks token usage and cache hit statistics.
///
/// Note: `last_cache` is only populated by Claude and `OpenAI` providers.
/// Ollama and Gemini only use `last_usage`.
///
/// `last_reasoning` stores reasoning tokens, which are a **subset** of completion tokens
/// (`OpenAI` o-series only). Never add reasoning tokens to cost separately.
#[derive(Debug, Default)]
#[allow(clippy::struct_field_names)] // all fields are `last_*` by design — they track the last seen value
pub(crate) struct UsageTracker {
    last_usage: std::sync::Mutex<Option<(u64, u64)>>,
    last_cache: std::sync::Mutex<Option<(u64, u64)>>,
    last_reasoning: std::sync::Mutex<Option<u64>>,
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
    last_ttft: std::sync::Mutex<Option<u64>>,
}

impl UsageTracker {
    pub(crate) fn record_usage(&self, input: u64, output: u64) {
        if let Ok(mut g) = self.last_usage.lock() {
            *g = Some((input, output));
        }
    }

    /// Record the time-to-first-token/byte (milliseconds) of the most recent HTTP attempt.
    pub(crate) fn record_ttft(&self, ms: u64) {
        if let Ok(mut g) = self.last_ttft.lock() {
            *g = Some(ms);
        }
    }

    pub(crate) fn record_cache(&self, creation: u64, read: u64) {
        if let Ok(mut g) = self.last_cache.lock() {
            *g = Some((creation, read));
        }
    }

    /// Record reasoning tokens from the last response.
    ///
    /// Reasoning tokens are a **subset** of completion tokens; callers must not add them
    /// to cost calculations.
    pub(crate) fn record_reasoning(&self, tokens: u64) {
        if let Ok(mut g) = self.last_reasoning.lock() {
            *g = Some(tokens);
        }
    }

    pub(crate) fn last_usage(&self) -> Option<(u64, u64)> {
        self.last_usage.lock().ok().and_then(|g| *g)
    }

    pub(crate) fn last_cache_usage(&self) -> Option<(u64, u64)> {
        self.last_cache.lock().ok().and_then(|g| *g)
    }

    /// Returns reasoning tokens from the last response, or `None` if the provider
    /// does not report them.
    pub(crate) fn last_reasoning(&self) -> Option<u64> {
        self.last_reasoning.lock().ok().and_then(|g| *g)
    }

    /// Returns the time-to-first-token/byte (milliseconds) of the last HTTP round-trip.
    pub(crate) fn last_ttft_ms(&self) -> Option<u64> {
        self.last_ttft.lock().ok().and_then(|g| *g)
    }
}

impl Clone for UsageTracker {
    fn clone(&self) -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn clone_resets_last_ttft_ms() {
        let tracker = UsageTracker::default();
        tracker.record_ttft(42);
        let cloned = tracker.clone();
        assert_eq!(
            cloned.last_ttft_ms(),
            None,
            "UsageTracker::clone must reset all state, including ttft"
        );
    }
}
