// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::VecDeque;

use tokio::sync::watch;
use zeph_common::SecurityEventCategory;

pub use zeph_llm::{ClassifierMetricsSnapshot, TaskMetricsSnapshot};
pub use zeph_memory::{CategoryScore, ProbeCategory, ProbeVerdict};

/// A single security event record for TUI display.
#[derive(Debug, Clone)]
pub struct SecurityEvent {
    /// Unix timestamp (seconds since epoch).
    pub timestamp: u64,
    pub category: SecurityEventCategory,
    /// Source that triggered the event (e.g., `web_scrape`, `mcp_response`).
    pub source: String,
    /// Short description, capped at 128 chars.
    pub detail: String,
}

impl SecurityEvent {
    #[must_use]
    pub fn new(
        category: SecurityEventCategory,
        source: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        // IMP-1: cap source at 64 chars and strip ASCII control chars.
        let source: String = source
            .into()
            .chars()
            .filter(|c| !c.is_ascii_control())
            .take(64)
            .collect();
        // CR-1: UTF-8 safe truncation using floor_char_boundary (stable since Rust 1.82).
        let detail = detail.into();
        let detail = if detail.len() > 128 {
            let end = detail.floor_char_boundary(127);
            format!("{}…", &detail[..end])
        } else {
            detail
        };
        Self {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            category,
            source,
            detail,
        }
    }
}

/// Ring buffer capacity for security events.
pub const SECURITY_EVENT_CAP: usize = 100;

/// Lightweight snapshot of a single task row for TUI display.
///
/// Captured from the task graph on each metrics tick; kept minimal on purpose.
#[derive(Debug, Clone)]
pub struct TaskSnapshotRow {
    pub id: u32,
    pub title: String,
    /// Stringified `TaskStatus` (e.g. `"pending"`, `"running"`, `"completed"`).
    pub status: String,
    pub agent: Option<String>,
    pub duration_ms: u64,
    /// Truncated error message (first 80 chars) when the task failed.
    pub error: Option<String>,
}

/// Lightweight snapshot of a `TaskGraph` for TUI display.
#[derive(Debug, Clone, Default)]
pub struct TaskGraphSnapshot {
    pub graph_id: String,
    pub goal: String,
    /// Stringified `GraphStatus` (e.g. `"created"`, `"running"`, `"completed"`).
    pub status: String,
    pub tasks: Vec<TaskSnapshotRow>,
    pub completed_at: Option<std::time::Instant>,
}

impl TaskGraphSnapshot {
    /// Returns `true` if this snapshot represents a terminal plan that finished
    /// more than 30 seconds ago and should no longer be shown in the TUI.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.completed_at
            .is_some_and(|t| t.elapsed().as_secs() > 30)
    }
}

/// Counters for the task orchestration subsystem.
///
/// Always present in [`MetricsSnapshot`]; zero-valued when orchestration is inactive.
#[derive(Debug, Clone, Default)]
pub struct OrchestrationMetrics {
    pub plans_total: u64,
    pub tasks_total: u64,
    pub tasks_completed: u64,
    pub tasks_failed: u64,
    pub tasks_skipped: u64,
}

/// Connection status of a single MCP server for TUI display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerConnectionStatus {
    Connected,
    Failed,
}

/// Per-server MCP status snapshot for TUI display.
#[derive(Debug, Clone)]
pub struct McpServerStatus {
    pub id: String,
    pub status: McpServerConnectionStatus,
    /// Number of tools provided by this server (0 when failed).
    pub tool_count: usize,
    /// Human-readable failure reason. Empty when connected.
    pub error: String,
}

/// Bayesian confidence data for a single skill, used by TUI confidence bar.
#[derive(Debug, Clone, Default)]
pub struct SkillConfidence {
    pub name: String,
    pub posterior: f64,
    pub total_uses: u32,
}

/// Snapshot of a single sub-agent's runtime status.
#[derive(Debug, Clone, Default)]
pub struct SubAgentMetrics {
    pub id: String,
    pub name: String,
    /// Stringified `TaskState`: "working", "completed", "failed", "canceled", etc.
    pub state: String,
    pub turns_used: u32,
    pub max_turns: u32,
    pub background: bool,
    pub elapsed_secs: u64,
    /// Stringified `PermissionMode`: `"default"`, `"accept_edits"`, `"dont_ask"`,
    /// `"bypass_permissions"`, `"plan"`. Empty string when mode is `Default`.
    pub permission_mode: String,
    /// Path to the directory containing this agent's JSONL transcript file.
    /// `None` when transcript writing is disabled for this agent.
    pub transcript_dir: Option<String>,
}

/// Per-turn latency breakdown for the four agent hot-path phases.
///
/// Populated with `Instant`-based measurements at each phase boundary.
/// All values are in milliseconds.
#[derive(Debug, Clone, Default)]
pub struct TurnTimings {
    pub prepare_context_ms: u64,
    pub llm_chat_ms: u64,
    pub tool_exec_ms: u64,
    pub persist_message_ms: u64,
}

/// Live snapshot of agent metrics broadcast via a [`tokio::sync::watch`] channel.
///
/// Fields are updated at different rates: some once at startup (static), others every turn
/// (dynamic). For fields that are known at agent startup and do not change during the session,
/// use [`StaticMetricsInit`] and `AgentBuilder::with_static_metrics` instead of
/// adding a raw `send_modify` call in the runner.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // independent boolean flags; bitflags or enum would obscure semantics without reducing complexity
pub struct MetricsSnapshot {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub context_tokens: u64,
    pub api_calls: u64,
    pub active_skills: Vec<String>,
    pub total_skills: usize,
    /// Total configured MCP servers (connected + failed).
    pub mcp_server_count: usize,
    pub mcp_tool_count: usize,
    /// Number of successfully connected MCP servers.
    pub mcp_connected_count: usize,
    /// Per-server connection status list.
    pub mcp_servers: Vec<McpServerStatus>,
    pub active_mcp_tools: Vec<String>,
    pub sqlite_message_count: u64,
    pub sqlite_conversation_id: Option<zeph_memory::ConversationId>,
    pub qdrant_available: bool,
    pub vector_backend: String,
    pub embeddings_generated: u64,
    pub last_llm_latency_ms: u64,
    pub uptime_seconds: u64,
    pub provider_name: String,
    pub model_name: String,
    pub summaries_count: u64,
    pub context_compactions: u64,
    /// Number of times the agent entered the Hard compaction tier, including cooldown-skipped
    /// turns. Not equal to the actual LLM summarization count — reflects pressure, not action.
    pub compaction_hard_count: u64,
    /// User-message turns elapsed after each hard compaction event.
    /// Entry i = turns between hard compaction i and hard compaction i+1 (or session end).
    /// Empty when no hard compaction occurred during the session.
    pub compaction_turns_after_hard: Vec<u64>,
    pub compression_events: u64,
    pub compression_tokens_saved: u64,
    pub tool_output_prunes: u64,
    /// Compaction probe outcomes (#1609).
    pub compaction_probe_passes: u64,
    /// Compaction probe soft failures (summary borderline — compaction proceeded with warning).
    pub compaction_probe_soft_failures: u64,
    /// Compaction probe hard failures (compaction blocked due to lossy summary).
    pub compaction_probe_failures: u64,
    /// Compaction probe errors (LLM/timeout — non-blocking, compaction proceeded).
    pub compaction_probe_errors: u64,
    /// Last compaction probe verdict. `None` before the first probe completes.
    pub last_probe_verdict: Option<zeph_memory::ProbeVerdict>,
    /// Last compaction probe score in [0.0, 1.0]. `None` before the first probe
    /// completes or after an Error verdict (errors produce no score).
    pub last_probe_score: Option<f32>,
    /// Per-category scores from the last completed probe.
    pub last_probe_category_scores: Option<Vec<zeph_memory::CategoryScore>>,
    /// Configured pass threshold for the compaction probe. Used by TUI for category color-coding.
    pub compaction_probe_threshold: f32,
    /// Configured hard-fail threshold for the compaction probe.
    pub compaction_probe_hard_fail_threshold: f32,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_spent_cents: f64,
    /// Cost per successful task in cents. `None` until at least one task completes.
    pub cost_cps_cents: Option<f64>,
    /// Number of successful tasks recorded today.
    pub cost_successful_tasks: u64,
    /// Per-provider cost breakdown, sorted by cost descending.
    pub provider_cost_breakdown: Vec<(String, crate::cost::ProviderUsage)>,
    pub filter_raw_tokens: u64,
    pub filter_saved_tokens: u64,
    pub filter_applications: u64,
    pub filter_total_commands: u64,
    pub filter_filtered_commands: u64,
    pub filter_confidence_full: u64,
    pub filter_confidence_partial: u64,
    pub filter_confidence_fallback: u64,
    pub cancellations: u64,
    pub server_compaction_events: u64,
    pub sanitizer_runs: u64,
    pub sanitizer_injection_flags: u64,
    /// Injection pattern hits on `ToolResult` (local) sources — likely false positives.
    ///
    /// Counts regex hits that fired on content from `shell`, `read_file`, `search_code`, etc.
    /// These sources are user-owned and not adversarial; a non-zero value indicates a pattern
    /// that needs tightening or a source reclassification.
    pub sanitizer_injection_fp_local: u64,
    pub sanitizer_truncations: u64,
    pub quarantine_invocations: u64,
    pub quarantine_failures: u64,
    /// ML classifier hard-blocked tool outputs (`enforcement_mode=block` only).
    pub classifier_tool_blocks: u64,
    /// ML classifier suspicious tool outputs (both enforcement modes).
    pub classifier_tool_suspicious: u64,
    /// `TurnCausalAnalyzer` flags: behavioral deviation detected at tool-return boundary.
    pub causal_ipi_flags: u64,
    /// VIGIL pre-sanitizer flags: tool outputs matched injection patterns (any action).
    pub vigil_flags_total: u64,
    /// VIGIL pre-sanitizer blocks: tool outputs replaced with sentinel (`strict_mode=true`).
    pub vigil_blocks_total: u64,
    pub exfiltration_images_blocked: u64,
    pub exfiltration_tool_urls_flagged: u64,
    pub exfiltration_memory_guards: u64,
    pub pii_scrub_count: u64,
    /// Number of times the PII NER classifier timed out; input fell back to regex-only.
    pub pii_ner_timeouts: u64,
    /// Number of times the PII NER circuit breaker tripped (disabled NER for the session).
    pub pii_ner_circuit_breaker_trips: u64,
    pub memory_validation_failures: u64,
    pub rate_limit_trips: u64,
    pub pre_execution_blocks: u64,
    pub pre_execution_warnings: u64,
    /// `true` when a guardrail filter is active for this session.
    pub guardrail_enabled: bool,
    /// `true` when guardrail is in warn-only mode (action = warn).
    pub guardrail_warn_mode: bool,
    pub sub_agents: Vec<SubAgentMetrics>,
    pub skill_confidence: Vec<SkillConfidence>,
    /// Scheduled task summaries: `[name, kind, mode, next_run]`.
    pub scheduled_tasks: Vec<[String; 4]>,
    /// Thompson Sampling distribution snapshots: `(provider, alpha, beta)`.
    pub router_thompson_stats: Vec<(String, f64, f64)>,
    /// Ring buffer of recent security events (cap 100, FIFO eviction).
    pub security_events: VecDeque<SecurityEvent>,
    pub orchestration: OrchestrationMetrics,
    /// Live snapshot of the currently active task graph. `None` when no plan is active.
    pub orchestration_graph: Option<TaskGraphSnapshot>,
    pub graph_community_detection_failures: u64,
    pub graph_entities_total: u64,
    pub graph_edges_total: u64,
    pub graph_communities_total: u64,
    pub graph_extraction_count: u64,
    pub graph_extraction_failures: u64,
    /// `true` when `config.llm.cloud.enable_extended_context = true`.
    /// Never set for other providers to avoid false positives.
    pub extended_context: bool,
    /// Latest compression-guidelines version (0 = no guidelines yet).
    pub guidelines_version: u32,
    /// ISO 8601 timestamp of the latest guidelines update (empty if none).
    pub guidelines_updated_at: String,
    pub tool_cache_hits: u64,
    pub tool_cache_misses: u64,
    pub tool_cache_entries: usize,
    /// Number of semantic-tier facts in memory (0 when tier promotion disabled).
    pub semantic_fact_count: u64,
    /// STT model name (e.g. "whisper-1"). `None` when STT is not configured.
    pub stt_model: Option<String>,
    /// Model used for context compaction/summarization. `None` when no summary provider is set.
    pub compaction_model: Option<String>,
    /// Temperature of the active provider when using Candle. `None` for API providers.
    pub provider_temperature: Option<f32>,
    /// Top-p of the active provider when using Candle. `None` for API providers.
    pub provider_top_p: Option<f32>,
    /// Embedding model name (e.g. `"nomic-embed-text"`). Empty when embeddings are disabled.
    pub embedding_model: String,
    /// Token budget for context window. `None` when not configured.
    pub token_budget: Option<u64>,
    /// Token threshold that triggers soft compaction. `None` when not configured.
    pub compaction_threshold: Option<u32>,
    /// Vault backend identifier: "age", "env", or "none".
    pub vault_backend: String,
    /// Active I/O channel name: `"cli"`, `"telegram"`, `"tui"`, `"discord"`, `"slack"`.
    pub active_channel: String,
    /// Background supervisor: inflight tasks across all classes.
    pub bg_inflight: u64,
    /// Background supervisor: total tasks dropped due to concurrency limit (all classes).
    pub bg_dropped: u64,
    /// Background supervisor: total tasks completed (all classes).
    pub bg_completed: u64,
    /// Background supervisor: inflight enrichment tasks.
    pub bg_enrichment_inflight: u64,
    /// Background supervisor: inflight telemetry tasks.
    pub bg_telemetry_inflight: u64,
    /// In-flight background shell runs. Empty when none are running or no `ShellExecutor` is wired.
    pub shell_background_runs: Vec<ShellBackgroundRunRow>,
    /// Whether self-learning (skill evolution) is enabled.
    pub self_learning_enabled: bool,
    /// Whether the semantic response cache is enabled.
    pub semantic_cache_enabled: bool,
    /// Whether semantic response caching is enabled (alias for `semantic_cache_enabled`).
    pub cache_enabled: bool,
    /// Whether assistant messages are auto-saved to memory.
    pub autosave_enabled: bool,
    /// Classifier p50/p95 latency metrics per task (injection, pii, feedback).
    pub classifier: ClassifierMetricsSnapshot,
    /// Latency breakdown for the most recently completed agent turn.
    pub last_turn_timings: TurnTimings,
    /// Rolling average of per-phase latency over the last 10 turns.
    pub avg_turn_timings: TurnTimings,
    /// Maximum per-phase latency observed within the rolling window (tail-latency visibility).
    ///
    /// M3: exposes `max_in_window` alongside the rolling average for operational monitoring.
    pub max_turn_timings: TurnTimings,
    /// Number of turns included in `avg_turn_timings` and `max_turn_timings` (capped at 10).
    pub timing_sample_count: u64,
    /// Total egress (outbound HTTP) requests attempted this session.
    pub egress_requests_total: u64,
    /// Egress events dropped due to bounded channel backpressure.
    pub egress_dropped_total: u64,
    /// Egress requests blocked by scheme/domain/SSRF policy.
    pub egress_blocked_total: u64,
    /// Runtime-resolved context window limit (tokens).
    ///
    /// Populated from `resolve_context_budget` after provider pool construction and refreshed on
    /// every `/provider` switch. `0` means unknown (pre-init or provider has no declared window);
    /// the TUI gauge renders `"—"` in this case to avoid divide-by-zero.
    pub context_max_tokens: u64,
    /// Token count at the time the most recent compaction was triggered. `0` = never compacted.
    pub compaction_last_before: u64,
    /// Token count after the most recent compaction completed. `0` = never compacted.
    pub compaction_last_after: u64,
    /// Unix epoch milliseconds when the most recent compaction occurred. `0` = never compacted.
    pub compaction_last_at_ms: u64,
}

/// Snapshot of a single in-flight background shell run for TUI display.
///
/// Populated from [`zeph_tools::BackgroundRunSnapshot`] during the per-turn metrics update in
/// `reap_background_tasks_and_update_metrics`. The `run_id` is truncated to 8 hex chars for
/// compact TUI rendering.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ShellBackgroundRunRow {
    /// First 8 hex characters of the run's UUID, sufficient for unique identification in TUI.
    pub run_id: String,
    /// Original command, truncated to 80 characters.
    pub command: String,
    /// Wall-clock elapsed seconds since spawn.
    pub elapsed_secs: u64,
}

/// Configuration-derived fields of [`MetricsSnapshot`] that are known at agent startup and do
/// not change during the session.
///
/// Pass this struct to `AgentBuilder::with_static_metrics` immediately after
/// `AgentBuilder::with_metrics` to initialize all static fields in one place rather than
/// through scattered `send_modify` calls in the runner.
///
/// # Examples
///
/// ```no_run
/// use zeph_core::metrics::StaticMetricsInit;
///
/// let init = StaticMetricsInit {
///     active_channel: "cli".to_owned(),
///     ..StaticMetricsInit::default()
/// };
/// ```
#[derive(Debug, Default)]
pub struct StaticMetricsInit {
    /// STT model name (e.g. `"whisper-1"`). `None` when STT is not configured.
    pub stt_model: Option<String>,
    /// Model used for context compaction/summarization. `None` when no summary provider is set.
    pub compaction_model: Option<String>,
    /// Whether the semantic response cache is enabled.
    ///
    /// This value is also written to [`MetricsSnapshot::cache_enabled`] which is an alias for the
    /// same concept.
    pub semantic_cache_enabled: bool,
    /// Embedding model name (e.g. `"nomic-embed-text"`). Empty when embeddings are disabled.
    pub embedding_model: String,
    /// Whether self-learning (skill evolution) is enabled.
    pub self_learning_enabled: bool,
    /// Active I/O channel name: `"cli"`, `"telegram"`, `"tui"`, `"discord"`, `"slack"`.
    pub active_channel: String,
    /// Token budget for context window. `None` when not configured.
    pub token_budget: Option<u64>,
    /// Token threshold that triggers soft compaction. `None` when not configured.
    pub compaction_threshold: Option<u32>,
    /// Vault backend identifier: `"age"`, `"env"`, or `"none"`.
    pub vault_backend: String,
    /// Whether assistant messages are auto-saved to memory.
    pub autosave_enabled: bool,
    /// Override for the active model name. When `Some`, replaces the model name set by the
    /// builder from `runtime.model_name` (which may be a placeholder) with the effective model
    /// resolved from the LLM provider configuration.
    pub model_name_override: Option<String>,
}

/// Strip ASCII control characters and ANSI escape sequences from a string for safe TUI display.
///
/// Allows tab, LF, and CR; removes everything else in the `0x00–0x1F` range including full
/// ANSI CSI sequences (`ESC[...`). This prevents escape-sequence injection from LLM planner
/// output into the TUI.
fn strip_ctrl(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Consume an ANSI CSI sequence: ESC [ <params> <final-byte in 0x40–0x7E>
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                for inner in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&inner) {
                        break;
                    }
                }
            }
            // Drop ESC and any consumed sequence — write nothing.
        } else if c.is_control() && c != '\t' && c != '\n' && c != '\r' {
            // drop other control chars
        } else {
            out.push(c);
        }
    }
    out
}

/// Convert a live `TaskGraph` into a lightweight snapshot for TUI display.
impl From<&zeph_orchestration::TaskGraph> for TaskGraphSnapshot {
    fn from(graph: &zeph_orchestration::TaskGraph) -> Self {
        let tasks = graph
            .tasks
            .iter()
            .map(|t| {
                let error = t
                    .result
                    .as_ref()
                    .filter(|_| t.status == zeph_orchestration::TaskStatus::Failed)
                    .and_then(|r| {
                        if r.output.is_empty() {
                            None
                        } else {
                            // Strip control chars, then truncate at 80 chars (SEC-P6-01).
                            let s = strip_ctrl(&r.output);
                            if s.len() > 80 {
                                let end = s.floor_char_boundary(79);
                                Some(format!("{}…", &s[..end]))
                            } else {
                                Some(s)
                            }
                        }
                    });
                let duration_ms = t.result.as_ref().map_or(0, |r| r.duration_ms);
                TaskSnapshotRow {
                    id: t.id.as_u32(),
                    title: strip_ctrl(&t.title),
                    status: t.status.to_string(),
                    agent: t.assigned_agent.as_deref().map(strip_ctrl),
                    duration_ms,
                    error,
                }
            })
            .collect();
        Self {
            graph_id: graph.id.to_string(),
            goal: strip_ctrl(&graph.goal),
            status: graph.status.to_string(),
            tasks,
            completed_at: None,
        }
    }
}

pub struct MetricsCollector {
    tx: watch::Sender<MetricsSnapshot>,
}

impl MetricsCollector {
    #[must_use]
    pub fn new() -> (Self, watch::Receiver<MetricsSnapshot>) {
        let (tx, rx) = watch::channel(MetricsSnapshot::default());
        (Self { tx }, rx)
    }

    pub fn update(&self, f: impl FnOnce(&mut MetricsSnapshot)) {
        self.tx.send_modify(f);
    }

    /// Publish the runtime-resolved context window limit.
    ///
    /// Call after the provider pool is constructed (builder) and on every successful
    /// `/provider` switch so the TUI context gauge reflects the active provider's window.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_core::metrics::MetricsCollector;
    ///
    /// let (collector, rx) = MetricsCollector::new();
    /// collector.set_context_max_tokens(128_000);
    /// assert_eq!(rx.borrow().context_max_tokens, 128_000);
    /// ```
    pub fn set_context_max_tokens(&self, max_tokens: u64) {
        self.tx.send_modify(|m| m.context_max_tokens = max_tokens);
    }

    /// Record the outcome of the most recent compaction event.
    ///
    /// Sets all three `compaction_last_*` fields atomically. `at_ms` is the Unix epoch in
    /// milliseconds — obtain via `SystemTime::UNIX_EPOCH.elapsed().unwrap_or_default().as_millis()`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_core::metrics::MetricsCollector;
    ///
    /// let (collector, rx) = MetricsCollector::new();
    /// collector.record_compaction(50_000, 12_000, 1_700_000_000_000);
    /// let snap = rx.borrow();
    /// assert_eq!(snap.compaction_last_before, 50_000);
    /// assert_eq!(snap.compaction_last_after, 12_000);
    /// assert_eq!(snap.compaction_last_at_ms, 1_700_000_000_000);
    /// ```
    pub fn record_compaction(&self, before: u64, after: u64, at_ms: u64) {
        self.tx.send_modify(|m| {
            m.compaction_last_before = before;
            m.compaction_last_after = after;
            m.compaction_last_at_ms = at_ms;
        });
    }

    /// Returns a clone of the underlying [`watch::Sender`].
    ///
    /// Use this to pass the sender to code that requires a raw
    /// `watch::Sender<MetricsSnapshot>` while the [`MetricsCollector`] is
    /// also shared (e.g., passed to a `MetricsBridge` layer).
    #[must_use]
    pub fn sender(&self) -> watch::Sender<MetricsSnapshot> {
        self.tx.clone()
    }
}

// ---------------------------------------------------------------------------
// HistogramRecorder
// ---------------------------------------------------------------------------

/// Per-event histogram recording contract for the agent loop.
///
/// Implementors record individual latency observations into Prometheus histograms
/// (or any other backend). The trait is object-safe: the agent stores an
/// `Option<Arc<dyn HistogramRecorder>>` and calls these methods at each measurement
/// point. When `None`, recording is a no-op with zero overhead.
///
/// # Contract for implementors
///
/// - All methods must be non-blocking; they must not call async code or acquire
///   mutexes that may block.
/// - Implementations must be `Send + Sync` — the agent loop runs on the tokio
///   thread pool and the recorder may be called from multiple tasks.
///
/// # Examples
///
/// ```rust
/// use std::sync::Arc;
/// use std::time::Duration;
/// use zeph_core::metrics::HistogramRecorder;
///
/// struct NoOpRecorder;
///
/// impl HistogramRecorder for NoOpRecorder {
///     fn observe_llm_latency(&self, _: Duration) {}
///     fn observe_turn_duration(&self, _: Duration) {}
///     fn observe_tool_execution(&self, _: Duration) {}
///     fn observe_bg_task(&self, _: &str, _: Duration) {}
/// }
///
/// let recorder: Arc<dyn HistogramRecorder> = Arc::new(NoOpRecorder);
/// recorder.observe_llm_latency(Duration::from_millis(500));
/// ```
pub trait HistogramRecorder: Send + Sync {
    /// Record a single LLM API call latency observation.
    fn observe_llm_latency(&self, duration: std::time::Duration);

    /// Record a full agent turn duration observation (context prep + LLM + tools + persist).
    fn observe_turn_duration(&self, duration: std::time::Duration);

    /// Record a single tool execution latency observation.
    fn observe_tool_execution(&self, duration: std::time::Duration);

    /// Record a background task completion latency.
    ///
    /// `class_label` is `"enrichment"` or `"telemetry"` (from `TaskClass::name()`).
    fn observe_bg_task(&self, class_label: &str, duration: std::time::Duration);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]

    use super::*;

    #[test]
    fn default_metrics_snapshot() {
        let m = MetricsSnapshot::default();
        assert_eq!(m.total_tokens, 0);
        assert_eq!(m.api_calls, 0);
        assert!(m.active_skills.is_empty());
        assert!(m.active_mcp_tools.is_empty());
        assert_eq!(m.mcp_tool_count, 0);
        assert_eq!(m.mcp_server_count, 0);
        assert!(m.provider_name.is_empty());
        assert_eq!(m.summaries_count, 0);
        // Phase 2 fields
        assert!(m.stt_model.is_none());
        assert!(m.compaction_model.is_none());
        assert!(m.provider_temperature.is_none());
        assert!(m.provider_top_p.is_none());
        assert!(m.active_channel.is_empty());
        assert!(m.embedding_model.is_empty());
        assert!(m.token_budget.is_none());
        assert!(!m.self_learning_enabled);
        assert!(!m.semantic_cache_enabled);
    }

    #[test]
    fn metrics_collector_update_phase2_fields() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| {
            m.stt_model = Some("whisper-1".into());
            m.compaction_model = Some("haiku".into());
            m.provider_temperature = Some(0.7);
            m.provider_top_p = Some(0.95);
            m.active_channel = "tui".into();
            m.embedding_model = "nomic-embed-text".into();
            m.token_budget = Some(200_000);
            m.self_learning_enabled = true;
            m.semantic_cache_enabled = true;
        });
        let s = rx.borrow();
        assert_eq!(s.stt_model.as_deref(), Some("whisper-1"));
        assert_eq!(s.compaction_model.as_deref(), Some("haiku"));
        assert_eq!(s.provider_temperature, Some(0.7));
        assert_eq!(s.provider_top_p, Some(0.95));
        assert_eq!(s.active_channel, "tui");
        assert_eq!(s.embedding_model, "nomic-embed-text");
        assert_eq!(s.token_budget, Some(200_000));
        assert!(s.self_learning_enabled);
        assert!(s.semantic_cache_enabled);
    }

    #[test]
    fn metrics_collector_update() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| {
            m.api_calls = 5;
            m.total_tokens = 1000;
        });
        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.api_calls, 5);
        assert_eq!(snapshot.total_tokens, 1000);
    }

    #[test]
    fn metrics_collector_multiple_updates() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| m.api_calls = 1);
        collector.update(|m| m.api_calls += 1);
        assert_eq!(rx.borrow().api_calls, 2);
    }

    #[test]
    fn metrics_snapshot_clone() {
        let mut m = MetricsSnapshot::default();
        m.provider_name = "ollama".into();
        let cloned = m.clone();
        assert_eq!(cloned.provider_name, "ollama");
    }

    #[test]
    fn filter_metrics_tracking() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| {
            m.filter_raw_tokens += 250;
            m.filter_saved_tokens += 200;
            m.filter_applications += 1;
        });
        collector.update(|m| {
            m.filter_raw_tokens += 100;
            m.filter_saved_tokens += 80;
            m.filter_applications += 1;
        });
        let s = rx.borrow();
        assert_eq!(s.filter_raw_tokens, 350);
        assert_eq!(s.filter_saved_tokens, 280);
        assert_eq!(s.filter_applications, 2);
    }

    #[test]
    fn filter_confidence_and_command_metrics() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| {
            m.filter_total_commands += 1;
            m.filter_filtered_commands += 1;
            m.filter_confidence_full += 1;
        });
        collector.update(|m| {
            m.filter_total_commands += 1;
            m.filter_confidence_partial += 1;
        });
        let s = rx.borrow();
        assert_eq!(s.filter_total_commands, 2);
        assert_eq!(s.filter_filtered_commands, 1);
        assert_eq!(s.filter_confidence_full, 1);
        assert_eq!(s.filter_confidence_partial, 1);
        assert_eq!(s.filter_confidence_fallback, 0);
    }

    #[test]
    fn summaries_count_tracks_summarizations() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| m.summaries_count += 1);
        collector.update(|m| m.summaries_count += 1);
        assert_eq!(rx.borrow().summaries_count, 2);
    }

    #[test]
    fn cancellations_counter_increments() {
        let (collector, rx) = MetricsCollector::new();
        assert_eq!(rx.borrow().cancellations, 0);
        collector.update(|m| m.cancellations += 1);
        collector.update(|m| m.cancellations += 1);
        assert_eq!(rx.borrow().cancellations, 2);
    }

    #[test]
    fn security_event_detail_exact_128_not_truncated() {
        let s = "a".repeat(128);
        let ev = SecurityEvent::new(SecurityEventCategory::InjectionFlag, "src", s.clone());
        assert_eq!(ev.detail, s, "128-char string must not be truncated");
    }

    #[test]
    fn security_event_detail_129_is_truncated() {
        let s = "a".repeat(129);
        let ev = SecurityEvent::new(SecurityEventCategory::InjectionFlag, "src", s);
        assert!(
            ev.detail.ends_with('…'),
            "129-char string must end with ellipsis"
        );
        assert!(
            ev.detail.len() <= 130,
            "truncated detail must be at most 130 bytes"
        );
    }

    #[test]
    fn security_event_detail_multibyte_utf8_no_panic() {
        // Each '中' is 3 bytes. 43 chars = 129 bytes — triggers truncation at a multi-byte boundary.
        let s = "中".repeat(43);
        let ev = SecurityEvent::new(SecurityEventCategory::InjectionFlag, "src", s);
        assert!(ev.detail.ends_with('…'));
    }

    #[test]
    fn security_event_source_capped_at_64_chars() {
        let long_source = "x".repeat(200);
        let ev = SecurityEvent::new(SecurityEventCategory::InjectionFlag, long_source, "detail");
        assert_eq!(ev.source.len(), 64);
    }

    #[test]
    fn security_event_source_strips_control_chars() {
        let source = "tool\x00name\x1b[31m";
        let ev = SecurityEvent::new(SecurityEventCategory::InjectionFlag, source, "detail");
        assert!(!ev.source.contains('\x00'));
        assert!(!ev.source.contains('\x1b'));
    }

    #[test]
    fn security_event_category_as_str() {
        assert_eq!(SecurityEventCategory::InjectionFlag.as_str(), "injection");
        assert_eq!(SecurityEventCategory::ExfiltrationBlock.as_str(), "exfil");
        assert_eq!(SecurityEventCategory::Quarantine.as_str(), "quarantine");
        assert_eq!(SecurityEventCategory::Truncation.as_str(), "truncation");
        assert_eq!(
            SecurityEventCategory::CrossBoundaryMcpToAcp.as_str(),
            "cross_boundary_mcp_to_acp"
        );
    }

    #[test]
    fn ring_buffer_respects_cap_via_update() {
        let (collector, rx) = MetricsCollector::new();
        for i in 0..110u64 {
            let event = SecurityEvent::new(
                SecurityEventCategory::InjectionFlag,
                "src",
                format!("event {i}"),
            );
            collector.update(|m| {
                if m.security_events.len() >= SECURITY_EVENT_CAP {
                    m.security_events.pop_front();
                }
                m.security_events.push_back(event);
            });
        }
        let snap = rx.borrow();
        assert_eq!(snap.security_events.len(), SECURITY_EVENT_CAP);
        // FIFO: earliest events evicted, last one present
        assert!(snap.security_events.back().unwrap().detail.contains("109"));
    }

    #[test]
    fn security_events_empty_by_default() {
        let m = MetricsSnapshot::default();
        assert!(m.security_events.is_empty());
    }

    #[test]
    fn orchestration_metrics_default_zero() {
        let m = OrchestrationMetrics::default();
        assert_eq!(m.plans_total, 0);
        assert_eq!(m.tasks_total, 0);
        assert_eq!(m.tasks_completed, 0);
        assert_eq!(m.tasks_failed, 0);
        assert_eq!(m.tasks_skipped, 0);
    }

    #[test]
    fn metrics_snapshot_includes_orchestration_default_zero() {
        let m = MetricsSnapshot::default();
        assert_eq!(m.orchestration.plans_total, 0);
        assert_eq!(m.orchestration.tasks_total, 0);
        assert_eq!(m.orchestration.tasks_completed, 0);
    }

    #[test]
    fn orchestration_metrics_update_via_collector() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| {
            m.orchestration.plans_total += 1;
            m.orchestration.tasks_total += 5;
            m.orchestration.tasks_completed += 3;
            m.orchestration.tasks_failed += 1;
            m.orchestration.tasks_skipped += 1;
        });
        let s = rx.borrow();
        assert_eq!(s.orchestration.plans_total, 1);
        assert_eq!(s.orchestration.tasks_total, 5);
        assert_eq!(s.orchestration.tasks_completed, 3);
        assert_eq!(s.orchestration.tasks_failed, 1);
        assert_eq!(s.orchestration.tasks_skipped, 1);
    }

    #[test]
    fn strip_ctrl_removes_escape_sequences() {
        let input = "hello\x1b[31mworld\x00end";
        let result = strip_ctrl(input);
        assert_eq!(result, "helloworldend");
    }

    #[test]
    fn strip_ctrl_allows_tab_lf_cr() {
        let input = "a\tb\nc\rd";
        let result = strip_ctrl(input);
        assert_eq!(result, "a\tb\nc\rd");
    }

    #[test]
    fn task_graph_snapshot_is_stale_after_30s() {
        let mut snap = TaskGraphSnapshot::default();
        // Not stale if no completed_at.
        assert!(!snap.is_stale());
        // Not stale if just completed.
        snap.completed_at = Some(std::time::Instant::now());
        assert!(!snap.is_stale());
        // Stale if completed more than 30s ago.
        snap.completed_at = Some(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(31))
                .unwrap(),
        );
        assert!(snap.is_stale());
    }

    // T1: From<&TaskGraph> correctly maps fields including duration_ms and error truncation.
    #[test]
    fn task_graph_snapshot_from_task_graph_maps_fields() {
        use zeph_orchestration::{GraphStatus, TaskGraph, TaskNode, TaskResult, TaskStatus};

        let mut graph = TaskGraph::new("My goal");
        let mut task = TaskNode::new(0, "Do work", "description");
        task.status = TaskStatus::Failed;
        task.assigned_agent = Some("agent-1".into());
        task.result = Some(TaskResult {
            output: "error occurred here".into(),
            artifacts: vec![],
            duration_ms: 1234,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks.push(task);
        graph.status = GraphStatus::Failed;

        let snap = TaskGraphSnapshot::from(&graph);
        assert_eq!(snap.goal, "My goal");
        assert_eq!(snap.status, "failed");
        assert_eq!(snap.tasks.len(), 1);
        let row = &snap.tasks[0];
        assert_eq!(row.title, "Do work");
        assert_eq!(row.status, "failed");
        assert_eq!(row.agent.as_deref(), Some("agent-1"));
        assert_eq!(row.duration_ms, 1234);
        assert!(row.error.as_deref().unwrap().contains("error occurred"));
    }

    // T2: From impl compiles with orchestration feature active.
    #[test]
    fn task_graph_snapshot_from_compiles_with_feature() {
        use zeph_orchestration::TaskGraph;
        let graph = TaskGraph::new("feature flag test");
        let snap = TaskGraphSnapshot::from(&graph);
        assert_eq!(snap.goal, "feature flag test");
        assert!(snap.tasks.is_empty());
        assert!(!snap.is_stale());
    }

    // T1-extra: long error is truncated with ellipsis.
    #[test]
    fn task_graph_snapshot_error_truncated_at_80_chars() {
        use zeph_orchestration::{TaskGraph, TaskNode, TaskResult, TaskStatus};

        let mut graph = TaskGraph::new("goal");
        let mut task = TaskNode::new(0, "t", "d");
        task.status = TaskStatus::Failed;
        task.result = Some(TaskResult {
            output: "e".repeat(100),
            artifacts: vec![],
            duration_ms: 0,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks.push(task);

        let snap = TaskGraphSnapshot::from(&graph);
        let err = snap.tasks[0].error.as_ref().unwrap();
        assert!(err.ends_with('…'), "truncated error must end with ellipsis");
        assert!(
            err.len() <= 83,
            "truncated error must not exceed 80 chars + ellipsis"
        );
    }

    // SEC-P6-01: control chars in task title are stripped.
    #[test]
    fn task_graph_snapshot_strips_control_chars_from_title() {
        use zeph_orchestration::{TaskGraph, TaskNode};

        let mut graph = TaskGraph::new("goal\x1b[31m");
        let task = TaskNode::new(0, "title\x00injected", "d");
        graph.tasks.push(task);

        let snap = TaskGraphSnapshot::from(&graph);
        assert!(!snap.goal.contains('\x1b'), "goal must not contain escape");
        assert!(
            !snap.tasks[0].title.contains('\x00'),
            "title must not contain null byte"
        );
    }

    #[test]
    fn graph_metrics_default_zero() {
        let m = MetricsSnapshot::default();
        assert_eq!(m.graph_entities_total, 0);
        assert_eq!(m.graph_edges_total, 0);
        assert_eq!(m.graph_communities_total, 0);
        assert_eq!(m.graph_extraction_count, 0);
        assert_eq!(m.graph_extraction_failures, 0);
    }

    #[test]
    fn graph_metrics_update_via_collector() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| {
            m.graph_entities_total = 5;
            m.graph_edges_total = 10;
            m.graph_communities_total = 2;
            m.graph_extraction_count = 7;
            m.graph_extraction_failures = 1;
        });
        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.graph_entities_total, 5);
        assert_eq!(snapshot.graph_edges_total, 10);
        assert_eq!(snapshot.graph_communities_total, 2);
        assert_eq!(snapshot.graph_extraction_count, 7);
        assert_eq!(snapshot.graph_extraction_failures, 1);
    }

    #[test]
    fn histogram_recorder_trait_is_object_safe() {
        use std::sync::Arc;
        use std::time::Duration;

        struct NoOpRecorder;
        impl HistogramRecorder for NoOpRecorder {
            fn observe_llm_latency(&self, _: Duration) {}
            fn observe_turn_duration(&self, _: Duration) {}
            fn observe_tool_execution(&self, _: Duration) {}
            fn observe_bg_task(&self, _: &str, _: Duration) {}
        }

        // Verify the trait can be used as a trait object (object-safe).
        let recorder: Arc<dyn HistogramRecorder> = Arc::new(NoOpRecorder);
        recorder.observe_llm_latency(Duration::from_millis(500));
        recorder.observe_turn_duration(Duration::from_secs(3));
        recorder.observe_tool_execution(Duration::from_millis(100));
    }
}
