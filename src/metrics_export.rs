// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Prometheus metrics export for Zeph.
//!
//! [`PrometheusMetrics`] owns an [`Arc<Registry>`] and all ~25 metric families derived from
//! [`MetricsSnapshot`].  Call [`PrometheusMetrics::sync`] periodically to push the current
//! snapshot values into the registry.  [`spawn_metrics_sync`] spawns the background task that
//! drives the sync loop.
//!
//! This module is compiled only when the `prometheus` feature is enabled.

#![cfg(feature = "prometheus")]

use std::sync::Arc;

use std::sync::atomic::AtomicU64;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;
use tokio::sync::watch;
use zeph_core::metrics::MetricsSnapshot;

/// Bucket boundaries for latency histograms, in seconds.
///
/// Covers the typical range for LLM calls (1–30 s), tool executions (0.01–60 s),
/// and full agent turns (1–120 s).  Using a single set keeps the implementation
/// uniform; callers can adjust bucket resolution in a future iteration once
/// real-world data is available.
const LATENCY_BUCKETS: &[f64] = &[0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0];

// ---------------------------------------------------------------------------
// Label structs
// ---------------------------------------------------------------------------

/// Label for token direction (prompt / completion / `cache_read` / `cache_create`).
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct DirectionLabels {
    direction: &'static str,
}

/// Label for agent turn phase.
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct PhaseLabels {
    phase: &'static str,
}

/// Label for compaction tier (soft / hard).
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct TierLabels {
    tier: &'static str,
}

/// Label for tool cache result (hit / miss).
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct CacheResultLabels {
    result: &'static str,
}

/// Label for quarantine result (invoked / failed).
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct QuarantineResultLabels {
    result: &'static str,
}

/// Label for orchestration task status (completed / failed / skipped).
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct TaskStatusLabels {
    status: &'static str,
}

/// Label for MCP server connection status (connected / failed).
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct McpStatusLabels {
    status: &'static str,
}

/// Label for background task supervisor state (inflight / dropped / completed).
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BgStateLabels {
    state: &'static str,
}

// ---------------------------------------------------------------------------
// Helper: counter delta with reset detection
// ---------------------------------------------------------------------------

/// Compute the delta between `current` and `prev` counter values.
///
/// If `current < prev` a session reset is assumed and `current` is treated as the absolute value
/// (i.e. the delta equals `current`).
fn counter_delta(current: u64, prev: u64) -> u64 {
    if current < prev {
        current
    } else {
        current - prev
    }
}

/// Same as [`counter_delta`] but for `f64` counters.
///
/// Both `current` and `prev` are expected to be finite. Non-finite values (NaN, infinity)
/// produced by upstream arithmetic on `cost_spent_cents` will be passed through unchanged;
/// `prometheus-client` will encode them as `0` via `AtomicU64` bit-cast.
fn counter_delta_f64(current: f64, prev: f64) -> f64 {
    if !current.is_finite() {
        return 0.0;
    }
    if current < prev {
        current
    } else {
        current - prev
    }
}

// ---------------------------------------------------------------------------
// PrometheusMetrics
// ---------------------------------------------------------------------------

/// Owns all Prometheus metric families and an [`Arc<Registry>`].
///
/// Construct via [`PrometheusMetrics::new`], then pass the [`Arc<Registry>`] to the gateway and
/// call [`PrometheusMetrics::sync`] periodically from [`spawn_metrics_sync`].
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use zeph::metrics_export::PrometheusMetrics;
///
/// let pm = Arc::new(PrometheusMetrics::new());
/// let registry = Arc::clone(&pm.registry);
/// ```
pub struct PrometheusMetrics {
    /// The shared registry passed to the gateway `/metrics` handler.
    pub registry: Arc<Registry>,

    // --- LLM metrics ---
    llm_tokens_total: Family<DirectionLabels, Counter>,
    llm_api_calls_total: Counter,
    /// NOTE: deferred — no per-provider labels in `MetricsSnapshot`; use f64 for fractional cents.
    llm_cost_cents_total: Counter<f64, AtomicU64>,
    llm_latency_ms: Gauge,
    llm_context_tokens: Gauge,

    // --- Turn phase metrics ---
    turn_phase_duration_ms: Family<PhaseLabels, Gauge>,
    turn_phase_avg_ms: Family<PhaseLabels, Gauge>,
    turn_phase_max_ms: Family<PhaseLabels, Gauge>,

    // --- Memory metrics ---
    memory_messages_total: Gauge,
    memory_embeddings_total: Counter,
    memory_summaries_total: Counter,
    memory_compactions_total: Family<TierLabels, Counter>,
    memory_qdrant_available: Gauge,

    // --- Tool metrics ---
    tool_cache_total: Family<CacheResultLabels, Counter>,
    tool_output_prunes_total: Counter,

    // --- Security metrics ---
    security_injection_flags_total: Counter,
    security_exfiltration_blocks_total: Counter,
    security_quarantine_total: Family<QuarantineResultLabels, Counter>,
    security_rate_limit_trips_total: Counter,

    // --- Orchestration metrics ---
    orchestration_plans_total: Counter,
    orchestration_tasks_total: Family<TaskStatusLabels, Counter>,

    // --- MCP metrics ---
    mcp_servers: Family<McpStatusLabels, Gauge>,

    // --- Background task supervisor metrics ---
    background_tasks: Family<BgStateLabels, Gauge>,

    // --- System metrics ---
    uptime_seconds: Gauge,
    skills_total: Gauge,

    // --- Histogram metrics (Phase 3) ---
    llm_latency_seconds: Histogram,
    turn_duration_seconds: Histogram,
    tool_execution_seconds: Histogram,
}

impl PrometheusMetrics {
    /// Create a new `PrometheusMetrics` instance and register all metric families.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// # #[cfg(feature = "prometheus")]
    /// let pm = Arc::new(zeph::metrics_export::PrometheusMetrics::new());
    /// ```
    #[allow(clippy::too_many_lines)] // flat registration of ~25 metric families — no meaningful split
    #[must_use]
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let llm_tokens_total = Family::<DirectionLabels, Counter>::default();
        registry.register(
            "zeph_llm_tokens",
            "Total LLM tokens consumed, partitioned by direction",
            llm_tokens_total.clone(),
        );

        let llm_api_calls_total = Counter::default();
        registry.register(
            "zeph_llm_api_calls",
            "Total LLM API calls made",
            llm_api_calls_total.clone(),
        );

        let llm_cost_cents_total: Counter<f64, AtomicU64> = Counter::default();
        registry.register(
            "zeph_llm_cost_cents",
            "Total LLM cost in fractional US cents",
            llm_cost_cents_total.clone(),
        );

        let llm_latency_ms = Gauge::default();
        registry.register(
            "zeph_llm_latency_ms",
            "Last LLM API call latency in milliseconds",
            llm_latency_ms.clone(),
        );

        let llm_context_tokens = Gauge::default();
        registry.register(
            "zeph_llm_context_tokens",
            "Current context window token count",
            llm_context_tokens.clone(),
        );

        let turn_phase_duration_ms = Family::<PhaseLabels, Gauge>::default();
        registry.register(
            "zeph_turn_phase_duration_ms",
            "Last agent turn phase duration in milliseconds",
            turn_phase_duration_ms.clone(),
        );

        let turn_phase_avg_ms = Family::<PhaseLabels, Gauge>::default();
        registry.register(
            "zeph_turn_phase_avg_ms",
            "Rolling average agent turn phase duration in milliseconds (last 10 turns)",
            turn_phase_avg_ms.clone(),
        );

        let turn_phase_max_ms = Family::<PhaseLabels, Gauge>::default();
        registry.register(
            "zeph_turn_phase_max_ms",
            "Maximum agent turn phase duration in milliseconds (last 10 turns)",
            turn_phase_max_ms.clone(),
        );

        let memory_messages_total = Gauge::default();
        registry.register(
            "zeph_memory_messages",
            "Number of messages stored in SQLite",
            memory_messages_total.clone(),
        );

        let memory_embeddings_total = Counter::default();
        registry.register(
            "zeph_memory_embeddings",
            "Total embeddings generated",
            memory_embeddings_total.clone(),
        );

        let memory_summaries_total = Counter::default();
        registry.register(
            "zeph_memory_summaries",
            "Total context summaries produced",
            memory_summaries_total.clone(),
        );

        let memory_compactions_total = Family::<TierLabels, Counter>::default();
        registry.register(
            "zeph_memory_compactions",
            "Total context compactions by tier (soft/hard)",
            memory_compactions_total.clone(),
        );

        let memory_qdrant_available = Gauge::default();
        registry.register(
            "zeph_memory_qdrant_available",
            "Whether the Qdrant vector store is reachable (1 = yes, 0 = no)",
            memory_qdrant_available.clone(),
        );

        let tool_cache_total = Family::<CacheResultLabels, Counter>::default();
        registry.register(
            "zeph_tool_cache",
            "Tool output cache hits and misses",
            tool_cache_total.clone(),
        );

        let tool_output_prunes_total = Counter::default();
        registry.register(
            "zeph_tool_output_prunes",
            "Total tool output prune events",
            tool_output_prunes_total.clone(),
        );

        let security_injection_flags_total = Counter::default();
        registry.register(
            "zeph_security_injection_flags",
            "Total prompt injection flags raised by the sanitizer",
            security_injection_flags_total.clone(),
        );

        let security_exfiltration_blocks_total = Counter::default();
        registry.register(
            "zeph_security_exfiltration_blocks",
            "Total exfiltration attempts blocked (image channel)",
            security_exfiltration_blocks_total.clone(),
        );

        let security_quarantine_total = Family::<QuarantineResultLabels, Counter>::default();
        registry.register(
            "zeph_security_quarantine",
            "Quarantine sandbox invocations and failures",
            security_quarantine_total.clone(),
        );

        let security_rate_limit_trips_total = Counter::default();
        registry.register(
            "zeph_security_rate_limit_trips",
            "Total rate-limit trips across all channels",
            security_rate_limit_trips_total.clone(),
        );

        let orchestration_plans_total = Counter::default();
        registry.register(
            "zeph_orchestration_plans",
            "Total orchestration plans created",
            orchestration_plans_total.clone(),
        );

        let orchestration_tasks_total = Family::<TaskStatusLabels, Counter>::default();
        registry.register(
            "zeph_orchestration_tasks",
            "Orchestration task outcomes by status (completed/failed/skipped)",
            orchestration_tasks_total.clone(),
        );

        let uptime_seconds = Gauge::default();
        registry.register(
            "zeph_uptime_seconds",
            "Agent uptime in seconds since the current session started",
            uptime_seconds.clone(),
        );

        let skills_total = Gauge::default();
        registry.register(
            "zeph_skills",
            "Total number of skills loaded in the registry",
            skills_total.clone(),
        );

        let mcp_servers = Family::<McpStatusLabels, Gauge>::default();
        registry.register(
            "zeph_mcp_servers",
            "Number of MCP servers by connection status (connected/failed)",
            mcp_servers.clone(),
        );

        let background_tasks = Family::<BgStateLabels, Gauge>::default();
        registry.register(
            "zeph_background_tasks",
            "Background task supervisor counts by state (inflight/dropped/completed)",
            background_tasks.clone(),
        );

        let llm_latency_seconds = Histogram::new(LATENCY_BUCKETS.iter().copied());
        registry.register(
            "zeph_llm_latency_seconds",
            "LLM API call latency distribution in seconds",
            llm_latency_seconds.clone(),
        );

        let turn_duration_seconds = Histogram::new(LATENCY_BUCKETS.iter().copied());
        registry.register(
            "zeph_turn_duration_seconds",
            "Full agent turn duration distribution in seconds (context + LLM + tools + persist)",
            turn_duration_seconds.clone(),
        );

        let tool_execution_seconds = Histogram::new(LATENCY_BUCKETS.iter().copied());
        registry.register(
            "zeph_tool_execution_seconds",
            "Individual tool execution latency distribution in seconds",
            tool_execution_seconds.clone(),
        );

        Self {
            registry: Arc::new(registry),
            llm_tokens_total,
            llm_api_calls_total,
            llm_cost_cents_total,
            llm_latency_ms,
            llm_context_tokens,
            turn_phase_duration_ms,
            turn_phase_avg_ms,
            turn_phase_max_ms,
            memory_messages_total,
            memory_embeddings_total,
            memory_summaries_total,
            memory_compactions_total,
            memory_qdrant_available,
            tool_cache_total,
            tool_output_prunes_total,
            security_injection_flags_total,
            security_exfiltration_blocks_total,
            security_quarantine_total,
            security_rate_limit_trips_total,
            orchestration_plans_total,
            orchestration_tasks_total,
            mcp_servers,
            background_tasks,
            uptime_seconds,
            skills_total,
            llm_latency_seconds,
            turn_duration_seconds,
            tool_execution_seconds,
        }
    }

    /// Synchronise metric values from `current` snapshot, computing counter deltas against `prev`.
    ///
    /// Gauges are set to the absolute value from `current`. Counters receive the delta
    /// `current - prev` (or `current` when `current < prev`, which indicates a session restart).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[cfg(feature = "prometheus")]
    /// # {
    /// use std::sync::Arc;
    /// use zeph_core::metrics::MetricsSnapshot;
    /// use zeph::metrics_export::PrometheusMetrics;
    ///
    /// let pm = Arc::new(PrometheusMetrics::new());
    /// let prev = MetricsSnapshot::default();
    /// let mut cur = MetricsSnapshot::default();
    /// cur.api_calls = 3;
    /// pm.sync(&cur, &prev);
    /// # }
    /// ```
    #[allow(clippy::too_many_lines)] // one delta update per metric family — no meaningful split
    pub fn sync(&self, current: &MetricsSnapshot, prev: &MetricsSnapshot) {
        // --- LLM tokens (counters with direction label) ---
        let prompt_delta = counter_delta(current.prompt_tokens, prev.prompt_tokens);
        let completion_delta = counter_delta(current.completion_tokens, prev.completion_tokens);
        let cache_read_delta = counter_delta(current.cache_read_tokens, prev.cache_read_tokens);
        let cache_create_delta =
            counter_delta(current.cache_creation_tokens, prev.cache_creation_tokens);

        if prompt_delta > 0 {
            self.llm_tokens_total
                .get_or_create(&DirectionLabels {
                    direction: "prompt",
                })
                .inc_by(prompt_delta);
        }
        if completion_delta > 0 {
            self.llm_tokens_total
                .get_or_create(&DirectionLabels {
                    direction: "completion",
                })
                .inc_by(completion_delta);
        }
        if cache_read_delta > 0 {
            self.llm_tokens_total
                .get_or_create(&DirectionLabels {
                    direction: "cache_read",
                })
                .inc_by(cache_read_delta);
        }
        if cache_create_delta > 0 {
            self.llm_tokens_total
                .get_or_create(&DirectionLabels {
                    direction: "cache_create",
                })
                .inc_by(cache_create_delta);
        }

        // --- LLM API calls ---
        let api_calls_delta = counter_delta(current.api_calls, prev.api_calls);
        if api_calls_delta > 0 {
            self.llm_api_calls_total.inc_by(api_calls_delta);
        }

        // --- LLM cost ---
        let cost_delta = counter_delta_f64(current.cost_spent_cents, prev.cost_spent_cents);
        if cost_delta > 0.0 {
            self.llm_cost_cents_total.inc_by(cost_delta);
        }

        // --- LLM gauges ---
        self.llm_latency_ms
            .set(i64::try_from(current.last_llm_latency_ms).unwrap_or(i64::MAX));
        self.llm_context_tokens
            .set(i64::try_from(current.context_tokens).unwrap_or(i64::MAX));

        // --- Turn phase gauges ---
        for (label, last, avg, max) in [
            (
                "prepare_context",
                current.last_turn_timings.prepare_context_ms,
                current.avg_turn_timings.prepare_context_ms,
                current.max_turn_timings.prepare_context_ms,
            ),
            (
                "llm_chat",
                current.last_turn_timings.llm_chat_ms,
                current.avg_turn_timings.llm_chat_ms,
                current.max_turn_timings.llm_chat_ms,
            ),
            (
                "tool_exec",
                current.last_turn_timings.tool_exec_ms,
                current.avg_turn_timings.tool_exec_ms,
                current.max_turn_timings.tool_exec_ms,
            ),
            (
                "persist",
                current.last_turn_timings.persist_message_ms,
                current.avg_turn_timings.persist_message_ms,
                current.max_turn_timings.persist_message_ms,
            ),
        ] {
            let lbl = PhaseLabels { phase: label };
            self.turn_phase_duration_ms
                .get_or_create(&lbl)
                .set(i64::try_from(last).unwrap_or(i64::MAX));
            self.turn_phase_avg_ms
                .get_or_create(&lbl)
                .set(i64::try_from(avg).unwrap_or(i64::MAX));
            self.turn_phase_max_ms
                .get_or_create(&lbl)
                .set(i64::try_from(max).unwrap_or(i64::MAX));
        }

        // --- Memory gauges and counters ---
        self.memory_messages_total
            .set(i64::try_from(current.sqlite_message_count).unwrap_or(i64::MAX));

        let embeddings_delta =
            counter_delta(current.embeddings_generated, prev.embeddings_generated);
        if embeddings_delta > 0 {
            self.memory_embeddings_total.inc_by(embeddings_delta);
        }

        let summaries_delta = counter_delta(current.summaries_count, prev.summaries_count);
        if summaries_delta > 0 {
            self.memory_summaries_total.inc_by(summaries_delta);
        }

        let soft_delta = counter_delta(current.context_compactions, prev.context_compactions);
        if soft_delta > 0 {
            self.memory_compactions_total
                .get_or_create(&TierLabels { tier: "soft" })
                .inc_by(soft_delta);
        }
        let hard_delta = counter_delta(current.compaction_hard_count, prev.compaction_hard_count);
        if hard_delta > 0 {
            self.memory_compactions_total
                .get_or_create(&TierLabels { tier: "hard" })
                .inc_by(hard_delta);
        }

        self.memory_qdrant_available
            .set(i64::from(current.qdrant_available));

        // --- Tool metrics ---
        let cache_hit_delta = counter_delta(current.tool_cache_hits, prev.tool_cache_hits);
        if cache_hit_delta > 0 {
            self.tool_cache_total
                .get_or_create(&CacheResultLabels { result: "hit" })
                .inc_by(cache_hit_delta);
        }
        let cache_miss_delta = counter_delta(current.tool_cache_misses, prev.tool_cache_misses);
        if cache_miss_delta > 0 {
            self.tool_cache_total
                .get_or_create(&CacheResultLabels { result: "miss" })
                .inc_by(cache_miss_delta);
        }

        let prunes_delta = counter_delta(current.tool_output_prunes, prev.tool_output_prunes);
        if prunes_delta > 0 {
            self.tool_output_prunes_total.inc_by(prunes_delta);
        }

        // --- Security counters ---
        let injection_delta = counter_delta(
            current.sanitizer_injection_flags,
            prev.sanitizer_injection_flags,
        );
        if injection_delta > 0 {
            self.security_injection_flags_total.inc_by(injection_delta);
        }

        let exfil_delta = counter_delta(
            current.exfiltration_images_blocked,
            prev.exfiltration_images_blocked,
        );
        if exfil_delta > 0 {
            self.security_exfiltration_blocks_total.inc_by(exfil_delta);
        }

        let quar_inv_delta =
            counter_delta(current.quarantine_invocations, prev.quarantine_invocations);
        if quar_inv_delta > 0 {
            self.security_quarantine_total
                .get_or_create(&QuarantineResultLabels { result: "invoked" })
                .inc_by(quar_inv_delta);
        }
        let quar_fail_delta = counter_delta(current.quarantine_failures, prev.quarantine_failures);
        if quar_fail_delta > 0 {
            self.security_quarantine_total
                .get_or_create(&QuarantineResultLabels { result: "failed" })
                .inc_by(quar_fail_delta);
        }

        let rate_delta = counter_delta(current.rate_limit_trips, prev.rate_limit_trips);
        if rate_delta > 0 {
            self.security_rate_limit_trips_total.inc_by(rate_delta);
        }

        // --- Orchestration counters ---
        let plans_delta = counter_delta(
            current.orchestration.plans_total,
            prev.orchestration.plans_total,
        );
        if plans_delta > 0 {
            self.orchestration_plans_total.inc_by(plans_delta);
        }

        for (label, current_val, prev_val) in [
            (
                "completed",
                current.orchestration.tasks_completed,
                prev.orchestration.tasks_completed,
            ),
            (
                "failed",
                current.orchestration.tasks_failed,
                prev.orchestration.tasks_failed,
            ),
            (
                "skipped",
                current.orchestration.tasks_skipped,
                prev.orchestration.tasks_skipped,
            ),
        ] {
            let delta = counter_delta(current_val, prev_val);
            if delta > 0 {
                self.orchestration_tasks_total
                    .get_or_create(&TaskStatusLabels { status: label })
                    .inc_by(delta);
            }
        }

        // --- MCP gauges ---
        self.mcp_servers
            .get_or_create(&McpStatusLabels {
                status: "connected",
            })
            .set(i64::try_from(current.mcp_connected_count).unwrap_or(i64::MAX));
        // failed = total configured minus connected
        let mcp_failed = current
            .mcp_server_count
            .saturating_sub(current.mcp_connected_count);
        self.mcp_servers
            .get_or_create(&McpStatusLabels { status: "failed" })
            .set(i64::try_from(mcp_failed).unwrap_or(i64::MAX));

        // --- Background task supervisor gauges ---
        self.background_tasks
            .get_or_create(&BgStateLabels { state: "inflight" })
            .set(i64::try_from(current.bg_inflight).unwrap_or(i64::MAX));
        self.background_tasks
            .get_or_create(&BgStateLabels { state: "dropped" })
            .set(i64::try_from(current.bg_dropped).unwrap_or(i64::MAX));
        self.background_tasks
            .get_or_create(&BgStateLabels { state: "completed" })
            .set(i64::try_from(current.bg_completed).unwrap_or(i64::MAX));

        // --- System gauges ---
        self.uptime_seconds
            .set(i64::try_from(current.uptime_seconds).unwrap_or(i64::MAX));
        self.skills_total
            .set(i64::try_from(current.total_skills).unwrap_or(i64::MAX));
    }
}

impl Default for PrometheusMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl zeph_core::metrics::HistogramRecorder for PrometheusMetrics {
    fn observe_llm_latency(&self, duration: std::time::Duration) {
        self.llm_latency_seconds.observe(duration.as_secs_f64());
    }

    fn observe_turn_duration(&self, duration: std::time::Duration) {
        self.turn_duration_seconds.observe(duration.as_secs_f64());
    }

    fn observe_tool_execution(&self, duration: std::time::Duration) {
        self.tool_execution_seconds.observe(duration.as_secs_f64());
    }
}

// ---------------------------------------------------------------------------
// Background sync task
// ---------------------------------------------------------------------------

/// Spawn a background task that periodically reads the `MetricsSnapshot` watch channel and
/// calls [`PrometheusMetrics::sync`].
///
/// `interval_secs` is clamped to a minimum of 1 second.  The task uses
/// [`tokio::time::MissedTickBehavior::Skip`] so slow syncs do not accumulate ticks.
///
/// Returns a [`tokio::task::JoinHandle`] that should be stored and awaited on shutdown.
///
/// # Examples
///
/// ```no_run
/// # #[cfg(feature = "prometheus")]
/// # {
/// use std::sync::Arc;
/// use tokio::sync::watch;
/// use zeph_core::metrics::MetricsSnapshot;
/// use zeph::metrics_export::{PrometheusMetrics, spawn_metrics_sync};
///
/// let pm = Arc::new(PrometheusMetrics::new());
/// let (_tx, rx) = watch::channel(MetricsSnapshot::default());
/// let _handle = spawn_metrics_sync(pm, rx, 5);
/// # }
/// ```
pub fn spawn_metrics_sync(
    metrics: Arc<PrometheusMetrics>,
    mut snapshot_rx: watch::Receiver<MetricsSnapshot>,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    let original = interval_secs;
    let interval_secs = original.max(1);
    if original == 0 {
        tracing::warn!("[metrics] sync_interval_secs=0 is invalid; clamped to 1 second");
    }

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut prev = MetricsSnapshot::default();

        loop {
            interval.tick().await;

            let current = snapshot_rx.borrow_and_update().clone();
            metrics.sync(&current, &prev);
            prev = current;
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_core::metrics::MetricsSnapshot;

    #[test]
    fn test_counter_delta_normal() {
        assert_eq!(counter_delta(105, 100), 5);
        assert_eq!(counter_delta(100, 100), 0);
        assert_eq!(counter_delta(50, 0), 50);
        assert_eq!(counter_delta(0, 0), 0);
    }

    #[test]
    fn test_counter_delta_reset() {
        // current < prev means session restart — treat current as absolute
        assert_eq!(counter_delta(20, 100), 20);
        assert_eq!(counter_delta(0, 50), 0);
    }

    #[test]
    fn test_counter_delta_f64_normal() {
        let delta = counter_delta_f64(1.5, 1.0);
        assert!((delta - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_counter_delta_f64_reset() {
        let delta = counter_delta_f64(0.3, 1.0);
        assert!((delta - 0.3).abs() < 1e-9);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_sync_no_double_count() {
        let pm = PrometheusMetrics::new();

        let mut snap1 = MetricsSnapshot::default();
        let mut snap2 = MetricsSnapshot::default();

        snap1.api_calls = 0;
        snap2.api_calls = 5;

        // First sync: delta = 5
        pm.sync(&snap2, &snap1);

        let snap3 = snap2.clone(); // identical to snap2

        // Second sync with same values: delta = 0, counter should not increase
        pm.sync(&snap3, &snap2);

        // We can verify the counter via encoding
        let mut buf = String::new();
        prometheus_client::encoding::text::encode(&mut buf, &pm.registry).unwrap();
        // The counter should appear once with value 5, not 10
        assert!(
            buf.contains("zeph_llm_api_calls_total 5"),
            "counter should be 5, got:\n{buf}"
        );
    }

    #[tokio::test]
    async fn test_clamp_interval() {
        // This verifies the clamping logic compiles and runs without panic
        // (interval_secs=0 should clamp to 1 without panic)
        let pm = Arc::new(PrometheusMetrics::new());
        let (tx, rx) = watch::channel(MetricsSnapshot::default());
        let handle = spawn_metrics_sync(pm, rx, 0);
        // Drop tx so the watch channel is closed; handle will finish on next tick
        drop(tx);
        // Abort the task — we just want to confirm no panic during construction
        handle.abort();
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_openmetrics_encoding_format() {
        let pm = PrometheusMetrics::new();

        let mut snap = MetricsSnapshot::default();
        snap.api_calls = 7;
        snap.uptime_seconds = 42;
        snap.prompt_tokens = 100;

        pm.sync(&snap, &MetricsSnapshot::default());

        let mut buf = String::new();
        prometheus_client::encoding::text::encode(&mut buf, &pm.registry).unwrap();

        assert!(
            buf.contains("zeph_llm_api_calls_total"),
            "missing api_calls metric"
        );
        assert!(buf.contains("zeph_uptime_seconds"), "missing uptime metric");
        assert!(
            buf.contains("direction=\"prompt\""),
            "missing direction label"
        );
        assert!(buf.ends_with("# EOF\n"), "OpenMetrics requires EOF marker");
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_mcp_and_bg_metrics_sync() {
        let pm = PrometheusMetrics::new();

        let mut snap = MetricsSnapshot::default();
        snap.mcp_server_count = 3;
        snap.mcp_connected_count = 2;
        snap.bg_inflight = 1;
        snap.bg_dropped = 0;
        snap.bg_completed = 10;

        pm.sync(&snap, &MetricsSnapshot::default());

        let mut buf = String::new();
        prometheus_client::encoding::text::encode(&mut buf, &pm.registry).unwrap();

        assert!(
            buf.contains("status=\"connected\""),
            "missing mcp connected label"
        );
        assert!(
            buf.contains("status=\"failed\""),
            "missing mcp failed label"
        );
        assert!(
            buf.contains("state=\"inflight\""),
            "missing bg inflight label"
        );
        assert!(
            buf.contains("state=\"completed\""),
            "missing bg completed label"
        );
    }

    #[test]
    fn test_histogram_observation() {
        use zeph_core::metrics::HistogramRecorder;

        let pm = PrometheusMetrics::new();

        pm.observe_llm_latency(std::time::Duration::from_secs_f64(2.5));
        pm.observe_turn_duration(std::time::Duration::from_secs_f64(8.0));
        pm.observe_tool_execution(std::time::Duration::from_secs_f64(0.3));

        let mut buf = String::new();
        prometheus_client::encoding::text::encode(&mut buf, &pm.registry).unwrap();

        assert!(
            buf.contains("zeph_llm_latency_seconds"),
            "missing llm latency histogram"
        );
        assert!(
            buf.contains("zeph_turn_duration_seconds"),
            "missing turn duration histogram"
        );
        assert!(
            buf.contains("zeph_tool_execution_seconds"),
            "missing tool execution histogram"
        );
        // Verify at least one bucket and the sum are encoded.
        assert!(buf.contains("_bucket"), "missing histogram buckets");
        assert!(buf.contains("_sum"), "missing histogram sum");
        assert!(buf.contains("_count"), "missing histogram count");
    }

    #[test]
    fn test_histogram_buckets_are_valid() {
        // All bucket boundaries must be positive and strictly increasing.
        assert!(!LATENCY_BUCKETS.is_empty(), "bucket list must not be empty");
        let mut prev = f64::NEG_INFINITY;
        for &b in LATENCY_BUCKETS {
            assert!(b > 0.0, "bucket boundary {b} must be positive");
            assert!(b > prev, "bucket boundaries must be strictly increasing");
            prev = b;
        }
    }

    #[test]
    fn test_histogram_recorder_trait_impl() {
        use zeph_core::metrics::HistogramRecorder;

        let pm = PrometheusMetrics::new();
        let recorder: &dyn HistogramRecorder = &pm;

        // These calls must not panic.
        recorder.observe_llm_latency(std::time::Duration::from_millis(100));
        recorder.observe_turn_duration(std::time::Duration::from_millis(5000));
        recorder.observe_tool_execution(std::time::Duration::from_millis(50));
    }
}
