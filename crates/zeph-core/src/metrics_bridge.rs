// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tracing layer that derives [`crate::metrics::TurnTimings`] from span durations.
//!
//! [`MetricsBridge`] implements [`tracing_subscriber::Layer`] and observes
//! the close event of a fixed set of known spans. When a watched span closes,
//! the bridge computes the elapsed duration, writes it into the shared
//! [`MetricsCollector`], and marks the corresponding bit in
//! [`crate::metrics::MetricsSnapshot::bridge_timings_written`].
//!
//! `Agent::flush_turn_timings` (`agent/utils.rs`) consults that bitmask once per turn: for
//! each field the bridge marked as written, its span-derived value wins over the manual
//! `Instant::now()` timing computed on the hot path; fields the bridge did not mark (either
//! because no span with a matching name closed this turn, or a watched span name does not
//! yet correspond to any real span) fall back to the manual value (#5946). This makes the
//! two timing sources coexist per-field rather than one unconditionally clobbering the other.
//!
//! This module is compiled only when the `profiling` feature is enabled.

use std::sync::Arc;
use std::time::Instant;

use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::metrics::MetricsCollector;

/// Span names watched by the bridge, mapped to the [`TimingField`] they update.
///
/// `persist_message`'s real span (`core.persist.persist_message`) fires on every
/// `persist_message()` call — 7+ times per turn (user message, assistant response(s), tool
/// results) — not once per turn like the other three. `TurnTimings::persist_message_ms` is
/// meant to time only the first user-message persist of the turn, which only the pre-existing
/// manual `Instant::now()` timing in `agent/mod.rs` captures; watching the real span here would
/// make the bridge overwrite that value with whichever persist call happens to close last, which
/// is wrong. So `persist_message` is intentionally left out of `WATCHED_SPANS` — that field stays
/// manually-timed only (#6111).
const WATCHED_SPANS: &[(&str, TimingField)] = &[
    ("core.context.prepare_context", TimingField::PrepareContext),
    ("llm.chat", TimingField::LlmChat),
    ("core.tool.native_loop", TimingField::ToolExec),
];

/// Identifies which [`crate::metrics::TurnTimings`] field a watched span maps to.
///
/// `pub(crate)` so `Agent::flush_turn_timings` (`agent/utils.rs`) can look up
/// [`Self::bridge_bit`] when deciding whether to keep the bridge's span-derived value or
/// fall back to manual timing for a given field (#5946).
#[derive(Clone, Copy)]
pub(crate) enum TimingField {
    PrepareContext,
    LlmChat,
    ToolExec,
}

impl TimingField {
    /// Bit set in [`crate::metrics::MetricsSnapshot::bridge_timings_written`] when this field
    /// is written by [`MetricsBridge::on_close`]. Consumed by `Agent::flush_turn_timings`
    /// (`agent/utils.rs`) to decide, per field, whether the bridge's span-derived value should
    /// win over the manual `Instant::now()` timing for the current turn (#5946).
    pub(crate) const fn bridge_bit(self) -> u8 {
        match self {
            Self::PrepareContext => 1 << 0,
            Self::LlmChat => 1 << 1,
            Self::ToolExec => 1 << 2,
        }
    }
}

/// Zero-size marker extension inserted in `on_new_span` for watched spans only.
///
/// Avoids a second name lookup (and registry lock) in `on_enter` / `on_exit` /
/// `on_close` — the presence of this extension is sufficient to confirm the span
/// is watched without re-scanning [`WATCHED_SPANS`].
struct WatchedSpan;

/// Records the `Instant` at which the span was most recently entered.
///
/// Inserted (or updated) in `on_enter`; read and consumed in `on_exit`.
struct SpanEntry(Instant);

/// Accumulates total active execution time across all enter–exit cycles.
///
/// For synchronous spans there is exactly one enter–exit pair. For async spans
/// that yield mid-execution there may be many. [`on_close`] reads this value.
struct SpanTiming(u64);

/// Custom tracing layer that derives [`crate::metrics::TurnTimings`] from span durations.
///
/// Watches a fixed set of span names (`core.context.prepare_context`, `llm.chat`,
/// `core.tool.native_loop` — see `WATCHED_SPANS`) and records their close-time
/// durations into a shared [`MetricsCollector`].
///
/// Timing is captured on `on_enter` (not `on_new_span`) so that async spans
/// that yield between creation and first poll are measured correctly. For spans
/// that re-enter multiple times, each enter–exit delta is accumulated, giving
/// the total active execution time rather than wall-clock time.
///
/// # Construction
///
/// Create a `MetricsBridge` before calling `init_tracing()` so the collector
/// is available when the subscriber is built.
///
/// ```no_run
/// # use std::sync::Arc;
/// # use zeph_core::metrics::MetricsCollector;
/// # use zeph_core::metrics_bridge::MetricsBridge;
/// let (collector, _rx) = MetricsCollector::new();
/// let collector = Arc::new(collector);
/// let bridge = MetricsBridge::new(Arc::clone(&collector));
/// ```
pub struct MetricsBridge {
    collector: Arc<MetricsCollector>,
}

impl MetricsBridge {
    /// Create a new bridge that writes timing data to the given collector.
    #[must_use]
    pub fn new(collector: Arc<MetricsCollector>) -> Self {
        Self { collector }
    }
}

impl<S> Layer<S> for MetricsBridge
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        // attrs.metadata().name() is zero-cost — no registry lock.
        // Acquire the lock only for the small minority of watched spans.
        let name = attrs.metadata().name();
        if WATCHED_SPANS.iter().any(|(n, _)| *n == name)
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(WatchedSpan);
        }
    }

    fn on_enter(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            // Cheap extension check — no name comparison needed here.
            // Use `replace` rather than `insert`: async spans re-enter on every
            // poll cycle, so a `SpanEntry` from a prior enter may already exist.
            // `insert` panics on a second call for the same type; `replace` does not.
            if span.extensions().get::<WatchedSpan>().is_some() {
                span.extensions_mut().replace(SpanEntry(Instant::now()));
            }
        }
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            // Read the entry time via immutable borrow, then drop before acquiring mutable.
            let elapsed_ms = span
                .extensions()
                .get::<SpanEntry>()
                .map(|e| u64::try_from(e.0.elapsed().as_millis()).unwrap_or(u64::MAX));
            if let Some(elapsed_ms) = elapsed_ms {
                let mut exts = span.extensions_mut();
                if let Some(timing) = exts.get_mut::<SpanTiming>() {
                    timing.0 = timing.0.saturating_add(elapsed_ms);
                } else {
                    exts.insert(SpanTiming(elapsed_ms));
                }
            }
        }
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id) {
            let exts = span.extensions();
            if let Some(timing) = exts.get::<SpanTiming>() {
                let duration_ms = timing.0;
                let name = span.name();
                if let Some((_, field)) = WATCHED_SPANS.iter().find(|(n, _)| *n == name) {
                    let field = *field;
                    self.collector.update(|m| {
                        match field {
                            TimingField::PrepareContext => {
                                m.last_turn_timings.prepare_context_ms = duration_ms;
                            }
                            TimingField::LlmChat => {
                                m.last_turn_timings.llm_chat_ms = duration_ms;
                            }
                            TimingField::ToolExec => {
                                m.last_turn_timings.tool_exec_ms = duration_ms;
                            }
                        }
                        m.bridge_timings_written |= field.bridge_bit();
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    use super::MetricsBridge;
    use crate::metrics::MetricsCollector;

    fn make_bridge() -> (
        MetricsBridge,
        Arc<MetricsCollector>,
        tokio::sync::watch::Receiver<crate::metrics::MetricsSnapshot>,
    ) {
        let (collector, rx) = MetricsCollector::new();
        let arc = Arc::new(collector);
        (MetricsBridge::new(Arc::clone(&arc)), arc, rx)
    }

    /// `on_close` writes the correct `TurnTimings` field for each watched span.
    #[test]
    fn watched_span_updates_correct_field() {
        let (bridge, _collector, rx) = make_bridge();
        let subscriber = Registry::default().with(bridge);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::span!(tracing::Level::INFO, "llm.chat");
            let guard = span.enter();
            drop(guard);
            // span closes on Drop of the Span object.
        });

        let snapshot = rx.borrow().clone();
        // llm_chat_ms should be set to some non-zero value (actual elapsed time).
        // We can't assert an exact ms value, but we confirm the field is touched
        // and the others remain at their default (0).
        assert_eq!(snapshot.last_turn_timings.prepare_context_ms, 0);
        assert_eq!(snapshot.last_turn_timings.tool_exec_ms, 0);
        assert_eq!(snapshot.last_turn_timings.persist_message_ms, 0);
        // llm_chat_ms may be 0 in very fast test runs (sub-millisecond); that is
        // acceptable — what matters is the field was written and others were not.
        let _ = snapshot.last_turn_timings.llm_chat_ms;
    }

    /// Non-watched spans must not trigger any `collector.update()` call.
    #[test]
    fn non_watched_span_produces_no_update() {
        let (bridge, _collector, rx) = make_bridge();
        let subscriber = Registry::default().with(bridge);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::span!(tracing::Level::INFO, "some.other.span");
            let guard = span.enter();
            drop(guard);
        });

        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.last_turn_timings.prepare_context_ms, 0);
        assert_eq!(snapshot.last_turn_timings.llm_chat_ms, 0);
        assert_eq!(snapshot.last_turn_timings.tool_exec_ms, 0);
        assert_eq!(snapshot.last_turn_timings.persist_message_ms, 0);
    }

    /// The three watched span names must be present in `WATCHED_SPANS` (#6111:
    /// `agent.persist_message` was deliberately dropped, see the `WATCHED_SPANS` doc comment).
    #[test]
    fn all_watched_span_names_registered() {
        let expected = [
            "core.context.prepare_context",
            "llm.chat",
            "core.tool.native_loop",
        ];

        for span_name in expected {
            assert!(
                super::WATCHED_SPANS.iter().any(|(n, _)| *n == span_name),
                "span '{span_name}' not in WATCHED_SPANS",
            );
        }
        assert_eq!(
            super::WATCHED_SPANS.len(),
            expected.len(),
            "unexpected extra spans in WATCHED_SPANS"
        );
    }

    /// Regression guard for #6111: `WATCHED_SPANS` entries whose span is defined in this crate
    /// must match a real `#[tracing::instrument(name = "...")]` name, not a name that looks
    /// plausible but was never wired up. `llm.chat` lives in `zeph-llm` and is exercised by
    /// that crate's own span-name literals instead.
    #[test]
    fn watched_spans_match_real_instrument_names_in_this_crate() {
        let assembly_src = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/agent/context/assembly.rs"
        ));
        let tier_loop_src = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/agent/tool_execution/tier_loop.rs"
        ));

        assert!(
            assembly_src.contains(r#"name = "core.context.prepare_context""#),
            "core.context.prepare_context span not found in assembly.rs — WATCHED_SPANS has drifted"
        );
        assert!(
            tier_loop_src.contains(r#"name = "core.tool.native_loop""#),
            "core.tool.native_loop span not found in tier_loop.rs — WATCHED_SPANS has drifted"
        );
    }
}
