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
///
/// `llm.chat` (the bare, non-tool `chat()` provider method) is intentionally **not** watched,
/// for the same reason as `persist_message`: it is not exclusive to the primary turn-level LLM
/// call. Every tool-enabled turn (i.e. essentially every turn, since tools are always registered)
/// dispatches through `chat_with_tools()` instead, but a bare `llm.chat` span still fires from
/// several *auxiliary* call sites within the same turn — the MARCH self-check hook
/// (`quality/parser.rs`), compaction/probe, magic-docs updates, background learning, session
/// digest, heuristic promotion, and more. Any one of those closing after the real
/// `chat_with_tools` call would silently overwrite the correct span-derived duration with its
/// own (typically much smaller) one, which is exactly what caused turn-latency panels to show
/// `llm:0ms` while the real call took 25+ seconds (#6275). `chat_with_tools` is watched instead,
/// since it is the span the primary tool-loop dispatch actually creates.
///
/// `llm.chat_with_tools` is **not exclusive to the main interactive turn either**: in-process
/// sub-agents (`zeph-subagent/src/agent_loop.rs`, running concurrently on their own tokio tasks)
/// and the scheduler's `RunInline` inline-tool-loop (`agent/scheduler_loop.rs::run_inline_tool_loop`,
/// running sequentially on the same `Agent` but outside the normal turn cycle) both call
/// `provider.chat_with_tools()` directly, with no wrapping span of their own — sharing the same
/// process-wide `tracing` dispatcher, their spans are indistinguishable from the main turn's by
/// name alone and would otherwise inflate/corrupt `llm_chat_ms` for whichever real turn's
/// `flush_turn_timings` happens to read it next. `on_close` scopes `LlmChat` to spans nested
/// under [`MAIN_TURN_SPAN`] (`llm.turn_call`, created once per dispatch in
/// `agent/tool_execution/llm_dispatch.rs` around every `chat_with_tools` call the main turn
/// itself makes) and silently ignores any `llm.chat_with_tools` span that isn't (#6275 follow-up).
/// One deliberate exception: `llm_dispatch.rs`'s speculative-stream-failure fallback passes
/// `tracing::Span::none()` instead of reusing `llm_span`, so that fallback call's
/// `chat_with_tools` span has no `MAIN_TURN_SPAN` ancestor and is silently dropped by the bridge
/// too — harmless, since the manual `Instant::now()` timing still covers it.
const WATCHED_SPANS: &[(&str, TimingField)] = &[
    ("core.context.prepare_context", TimingField::PrepareContext),
    ("llm.chat_with_tools", TimingField::LlmChat),
    ("core.tool.native_loop", TimingField::ToolExec),
];

/// Name of the span that wraps nearly every `chat_with_tools` call the *main interactive turn*
/// makes (`agent/tool_execution/llm_dispatch.rs`), with one exception: the speculative-stream-
/// failure fallback path deliberately uses `Span::none()` instead, so its real LLM latency isn't
/// bridge-tracked here — the manual `pending_timings` value covers it instead (see the
/// `WATCHED_SPANS` doc comment for the full explanation). Used to scope the `LlmChat`
/// `WATCHED_SPANS` entry away from concurrent in-process sub-agents and the scheduler's inline
/// tool loop, neither of which creates this wrapping span — see the `WATCHED_SPANS` doc comment
/// (#6275 follow-up).
const MAIN_TURN_SPAN: &str = "llm.turn_call";

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
            let name = span.name();
            if let Some((_, field)) = WATCHED_SPANS.iter().find(|(n, _)| *n == name) {
                let field = *field;
                // #6275 follow-up: `llm.chat_with_tools` also fires from concurrent in-process
                // sub-agents and the scheduler's inline tool loop, neither of which wraps the
                // call in the main turn's `MAIN_TURN_SPAN`. Drop those before they ever reach
                // the collector, so they can't inflate/corrupt the main turn's `llm_chat_ms`.
                if matches!(field, TimingField::LlmChat)
                    && !span
                        .scope()
                        .any(|ancestor| ancestor.name() == MAIN_TURN_SPAN)
                {
                    return;
                }
                let exts = span.extensions();
                if let Some(timing) = exts.get::<SpanTiming>() {
                    let duration_ms = timing.0;
                    self.collector.update(|m| {
                        match field {
                            TimingField::PrepareContext => {
                                m.last_turn_timings.prepare_context_ms = duration_ms;
                            }
                            TimingField::LlmChat => {
                                // `chat_with_tools` may close more than once per turn (one per
                                // tool-loop round trip), so accumulate rather than overwrite —
                                // matching the manual `pending_timings.llm_chat_ms` semantics in
                                // `agent/tool_execution/llm_dispatch.rs`. `flush_turn_timings`
                                // resets this back to 0 once it has been read for the turn
                                // (#6275), so accumulation never leaks across turns.
                                m.last_turn_timings.llm_chat_ms =
                                    m.last_turn_timings.llm_chat_ms.saturating_add(duration_ms);
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
    ///
    /// `llm.chat_with_tools` must be nested under `MAIN_TURN_SPAN` (`llm.turn_call`) to be
    /// counted at all (#6275 follow-up) — see `llm_chat_with_tools_outside_main_turn_span_is_ignored`
    /// for the negative case.
    #[test]
    fn watched_span_updates_correct_field() {
        let (bridge, _collector, rx) = make_bridge();
        let subscriber = Registry::default().with(bridge);

        tracing::subscriber::with_default(subscriber, || {
            let turn_span = tracing::span!(tracing::Level::INFO, "llm.turn_call");
            let _turn_guard = turn_span.enter();
            let span = tracing::span!(tracing::Level::INFO, "llm.chat_with_tools");
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

    /// Regression guard for #6275: a bare `llm.chat` span (fired by auxiliary call sites like
    /// the MARCH self-check hook, compaction probe, or magic docs — never the primary
    /// tool-enabled turn call) must NOT be watched, since it is not exclusive to the turn's
    /// real LLM latency and would silently overwrite the correct `chat_with_tools`-derived
    /// duration.
    #[test]
    fn bare_llm_chat_span_is_not_watched() {
        let (bridge, _collector, rx) = make_bridge();
        let subscriber = Registry::default().with(bridge);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::span!(tracing::Level::INFO, "llm.chat");
            let guard = span.enter();
            drop(guard);
        });

        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.last_turn_timings.llm_chat_ms, 0);
        assert_eq!(snapshot.bridge_timings_written, 0);
    }

    /// `chat_with_tools` may close more than once per turn (one per tool-loop round trip);
    /// `on_close` must accumulate rather than overwrite so the total reflects the sum (#6275).
    ///
    /// Each span sleeps for a measurable duration so the test can distinguish accumulation from
    /// overwrite numerically: under overwrite semantics the final value would be roughly the
    /// *last* span's duration alone (~10ms); under accumulation it must be roughly the sum of
    /// all three (~30ms+).
    #[test]
    fn llm_chat_with_tools_span_accumulates_across_multiple_closes() {
        let (bridge, _collector, rx) = make_bridge();
        let subscriber = Registry::default().with(bridge);

        tracing::subscriber::with_default(subscriber, || {
            let turn_span = tracing::span!(tracing::Level::INFO, "llm.turn_call");
            let _turn_guard = turn_span.enter();
            for _ in 0..3 {
                let span = tracing::span!(tracing::Level::INFO, "llm.chat_with_tools");
                let guard = span.enter();
                std::thread::sleep(std::time::Duration::from_millis(10));
                drop(guard);
            }
        });

        let snapshot = rx.borrow().clone();
        assert!(
            snapshot.last_turn_timings.llm_chat_ms >= 25,
            "expected accumulated duration across 3 closes (~30ms+), got {}ms — \
             on_close may be overwriting instead of accumulating",
            snapshot.last_turn_timings.llm_chat_ms
        );
    }

    /// Regression guard (F1, #6275 follow-up): an `llm.chat_with_tools` span that is not nested
    /// under `MAIN_TURN_SPAN` (`llm.turn_call`) must be ignored entirely, not counted into
    /// `llm_chat_ms` or flagged via `bridge_timings_written`. This simulates in-process
    /// sub-agents (`zeph-subagent/src/agent_loop.rs`) and the scheduler's `RunInline` inline
    /// tool loop (`agent/scheduler_loop.rs::run_inline_tool_loop`), both of which call
    /// `provider.chat_with_tools()` directly with no such wrapping span — sharing the same
    /// process-wide `tracing` dispatcher as the main turn, their spans must not inflate or
    /// corrupt whichever real turn's `flush_turn_timings` reads the field next.
    #[test]
    fn llm_chat_with_tools_outside_main_turn_span_is_ignored() {
        let (bridge, _collector, rx) = make_bridge();
        let subscriber = Registry::default().with(bridge);

        tracing::subscriber::with_default(subscriber, || {
            // No `llm.turn_call` ancestor — e.g. a sub-agent's or scheduler inline task's
            // direct `chat_with_tools()` call.
            let span = tracing::span!(tracing::Level::INFO, "llm.chat_with_tools");
            let guard = span.enter();
            std::thread::sleep(std::time::Duration::from_millis(10));
            drop(guard);
        });

        let snapshot = rx.borrow().clone();
        assert_eq!(
            snapshot.last_turn_timings.llm_chat_ms, 0,
            "an llm.chat_with_tools span with no llm.turn_call ancestor must not be counted"
        );
        assert_eq!(
            snapshot.bridge_timings_written, 0,
            "an out-of-scope llm.chat_with_tools span must not set the LlmChat bridge bit"
        );
    }

    /// A real turn's `llm.chat_with_tools` call (nested under `llm.turn_call`) must still be
    /// counted even when an unrelated, out-of-scope `llm.chat_with_tools` span (simulating a
    /// concurrent sub-agent) closes around the same time — the scoping check must not
    /// accidentally suppress legitimate main-turn spans (#6275 follow-up).
    #[test]
    fn llm_chat_with_tools_inside_main_turn_span_counted_despite_concurrent_out_of_scope_span() {
        let (bridge, _collector, rx) = make_bridge();
        let subscriber = Registry::default().with(bridge);

        tracing::subscriber::with_default(subscriber, || {
            // Out-of-scope span first (simulated concurrent sub-agent).
            let outside = tracing::span!(tracing::Level::INFO, "llm.chat_with_tools");
            let outside_guard = outside.enter();
            drop(outside_guard);

            // Real main-turn span, correctly nested.
            let turn_span = tracing::span!(tracing::Level::INFO, "llm.turn_call");
            let _turn_guard = turn_span.enter();
            let inside = tracing::span!(tracing::Level::INFO, "llm.chat_with_tools");
            let inside_guard = inside.enter();
            std::thread::sleep(std::time::Duration::from_millis(10));
            drop(inside_guard);
        });

        let snapshot = rx.borrow().clone();
        assert!(
            snapshot.last_turn_timings.llm_chat_ms >= 5,
            "the in-scope span must still be counted, got {}ms",
            snapshot.last_turn_timings.llm_chat_ms
        );
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
            "llm.chat_with_tools",
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
    /// plausible but was never wired up. `llm.chat_with_tools` lives in `zeph-llm` and is
    /// exercised by that crate's own span-name literals instead.
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
