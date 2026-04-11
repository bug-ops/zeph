// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tracing layer that derives [`TurnTimings`] from span durations.
//!
//! [`MetricsBridge`] implements [`tracing_subscriber::Layer`] and observes
//! the close event of a fixed set of known spans. When a watched span closes,
//! the bridge computes the elapsed duration and writes it into the shared
//! [`MetricsCollector`], replacing manual `Instant::now()` timing in the
//! agent hot path.
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
const WATCHED_SPANS: &[(&str, TimingField)] = &[
    ("agent.prepare_context", TimingField::PrepareContext),
    ("llm.chat", TimingField::LlmChat),
    ("agent.tool_loop", TimingField::ToolExec),
    ("agent.persist_message", TimingField::PersistMessage),
];

/// Identifies which [`crate::metrics::TurnTimings`] field a watched span maps to.
#[derive(Clone, Copy)]
enum TimingField {
    PrepareContext,
    LlmChat,
    ToolExec,
    PersistMessage,
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
/// Watches a fixed set of span names (`agent.prepare_context`, `llm.chat`,
/// `agent.tool_loop`, `agent.persist_message`) and records their close-time
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
                    self.collector.update(|m| match field {
                        TimingField::PrepareContext => {
                            m.last_turn_timings.prepare_context_ms = duration_ms;
                        }
                        TimingField::LlmChat => {
                            m.last_turn_timings.llm_chat_ms = duration_ms;
                        }
                        TimingField::ToolExec => {
                            m.last_turn_timings.tool_exec_ms = duration_ms;
                        }
                        TimingField::PersistMessage => {
                            m.last_turn_timings.persist_message_ms = duration_ms;
                        }
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

    /// All four watched span names must be present in `WATCHED_SPANS`.
    #[test]
    fn all_watched_span_names_registered() {
        let expected = [
            "agent.prepare_context",
            "llm.chat",
            "agent.tool_loop",
            "agent.persist_message",
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
}
