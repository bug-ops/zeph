// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `ShadowSentinel`: persistent safety memory stream + LLM-based pre-execution probe.
//!
//! Extends [`TrajectorySentinel`](crate::agent::trajectory) (Phase 1, spec 050) with:
//!
//! 1. **Persistent event stream**: `safety_shadow_events` table stores ALL safety-relevant
//!    events across sessions (not limited to the last 8 turns like the in-memory sentinel).
//! 2. **[`SafetyProbe`] trait**: before high-risk tool categories (shell, file write, exfil-
//!    capable MCP tools), an LLM evaluates the full trajectory context and approves/denies.
//!
//! `ShadowSentinel` is **defence-in-depth only** — it is NOT the primary security gate.
//! `PolicyGateExecutor` and `TrajectorySentinel` remain the primary enforcement mechanisms
//! and continue to run regardless of probe results or timeouts.
//!
//! # Fail-open default
//!
//! `deny_on_timeout = false` (default) means a probe timeout or LLM error results in
//! [`ProbeVerdict::Allow`]. This is correct because:
//!
//! - `ShadowSentinel` is defence-in-depth; policy gate still runs after it.
//! - Failing closed on timeout would allow a `DoS`: slow context → every high-risk tool blocked.
//! - Operators who want fail-closed can set `deny_on_timeout = true` in config.
//!
//! # LLM isolation invariant
//!
//! The probe prompt MUST NEVER include the `TrajectorySentinel` score or risk level.
//! Exposing internal risk scores to the LLM would allow prompt injection attacks that
//! manipulate probe verdicts by crafting tool outputs to lower the perceived risk level.

use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use serde_json::Value as JsonValue;
use tracing::{Instrument as _, info_span};
use zeph_db::{DbPool, sql};
use zeph_llm::LlmProvider;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{Message, Role};

use zeph_common::SessionId;

use crate::agent::error::AgentError;

// ── Risk category ────────────────────────────────────────────────────────────

/// Classifies a tool into a risk tier for probe gating.
///
/// Only `Shell`, `FileWrite`, and `ExfilCapable` tools trigger a safety probe.
/// `Low` tools bypass the probe entirely, adding zero latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolRiskCategory {
    /// Shell execution — arbitrary commands, highest risk.
    Shell,
    /// File write or delete operations — persistent side effects.
    FileWrite,
    /// Network-capable MCP tools that could exfiltrate data.
    ExfilCapable,
    /// All other tools — probe is skipped.
    Low,
}

// ── Probe verdict ─────────────────────────────────────────────────────────────

/// Result of a `SafetyProbe` evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProbeVerdict {
    /// Tool execution is safe to proceed.
    Allow,
    /// Tool execution is denied. The `reason` is LLM-generated and returned to the
    /// agent loop as the tool result so the model can adapt its strategy.
    Deny {
        /// Human-readable explanation from the safety probe.
        reason: String,
    },
    /// Probe was skipped — tool is not in a high-risk category, feature is disabled,
    /// or the per-turn probe budget was exhausted.
    Skip,
}

// ── Sentinel event ───────────────────────────────────────────────────────────

/// A single probe trajectory record in the persistent safety sentinel stream.
///
/// Stored in `safety_shadow_events` and retrieved for cross-session probe context.
#[derive(Debug, Clone)]
pub struct SentinelEvent {
    /// Database row id (0 for unsaved records).
    pub id: i64,
    /// Agent session identifier.
    pub session_id: SessionId,
    /// Turn number within the session.
    pub turn_number: u64,
    /// Event category: `"tool_call"`, `"tool_result"`, `"risk_signal"`, `"probe_result"`.
    pub event_type: String,
    /// Fully-qualified tool id for tool events, `None` for non-tool events.
    pub tool_id: Option<String>,
    /// Serialised risk signal variant (from `TrajectorySentinel`), if applicable.
    pub risk_signal: Option<String>,
    /// Risk level at the time of the event: `"calm"`, `"elevated"`, `"high"`, `"critical"`.
    pub risk_level: String,
    /// Probe verdict for `probe_result` events: `"allow"`, `"deny"`, `"skip"`.
    pub probe_verdict: Option<String>,
    /// Short human-readable summary included in the LLM probe context.
    pub context_summary: Option<String>,
    /// Unix timestamp (seconds) when the event was recorded.
    pub created_at: i64,
}

// ── SafetyProbe trait ─────────────────────────────────────────────────────────

/// LLM-based pre-execution safety evaluator.
///
/// Implementors receive the full trajectory context and the proposed tool call
/// and return a [`ProbeVerdict`]. The probe runs BEFORE [`zeph_tools::PolicyGateExecutor`].
///
/// # Contract
///
/// - Probe timeout is mandatory (configured via `probe_timeout_ms`).
/// - Probe failure (LLM error, timeout when `deny_on_timeout = false`) results in `Allow`.
/// - Probe results are persisted to `safety_shadow_events` for cross-session learning.
/// - The probe prompt MUST NOT include the sentinel score or risk level (LLM isolation).
///
/// Uses `Pin<Box<dyn Future>>` returns for dyn-compatibility (stored as `Box<dyn SafetyProbe>`).
pub trait SafetyProbe: Send + Sync {
    /// Evaluate whether the proposed tool call is safe given the trajectory context.
    ///
    /// # Arguments
    ///
    /// * `tool_id` — fully-qualified tool identifier (e.g. `"builtin:shell"`).
    /// * `tool_args` — JSON arguments for the tool call.
    /// * `trajectory` — recent shadow events for context (last N events from the store).
    fn evaluate<'a>(
        &'a self,
        tool_id: &'a str,
        tool_args: &'a JsonValue,
        trajectory: &'a [SentinelEvent],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeVerdict> + Send + 'a>>;
}

// ── LlmSafetyProbe ───────────────────────────────────────────────────────────

/// LLM-backed implementation of `SafetyProbe`.
///
/// Sends a structured prompt to an LLM asking it to evaluate whether the proposed
/// tool call is safe given the trajectory. The prompt is intentionally minimal and
/// never includes internal risk scores (LLM isolation invariant).
pub struct LlmSafetyProbe {
    provider: Arc<AnyProvider>,
    timeout_ms: u64,
    deny_on_timeout: bool,
}

impl LlmSafetyProbe {
    /// Create a new `LlmSafetyProbe`.
    ///
    /// # Arguments
    ///
    /// * `provider` — LLM provider used for probe evaluation.
    /// * `timeout_ms` — maximum milliseconds to wait for the LLM response.
    /// * `deny_on_timeout` — when `true`, a timeout results in `Deny`; when `false`, `Allow`.
    #[must_use]
    pub fn new(provider: Arc<AnyProvider>, timeout_ms: u64, deny_on_timeout: bool) -> Self {
        Self {
            provider,
            timeout_ms,
            deny_on_timeout,
        }
    }

    fn build_prompt(
        tool_id: &str,
        tool_args: &JsonValue,
        trajectory: &[SentinelEvent],
    ) -> Vec<Message> {
        let context = if trajectory.is_empty() {
            "No prior events in this session.".to_owned()
        } else {
            trajectory
                .iter()
                .map(|e| {
                    format!(
                        "[turn {}] {} | tool={} | {}",
                        e.turn_number,
                        e.event_type,
                        e.tool_id.as_deref().unwrap_or("-"),
                        e.context_summary.as_deref().unwrap_or(""),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let args_preview = serde_json::to_string(tool_args)
            .unwrap_or_default()
            .chars()
            .take(500)
            .collect::<String>();

        let system = "You are a safety evaluator for an AI agent. \
            Evaluate whether the proposed tool call is safe to execute given the session context. \
            Reply ONLY with a JSON object: {\"verdict\": \"allow\"} or {\"verdict\": \"deny\", \"reason\": \"...\"}. \
            Deny only if there is clear evidence of harmful intent or a dangerous pattern. \
            When uncertain, allow.";

        let user =
            format!("Tool: {tool_id}\nArgs: {args_preview}\n\nRecent session events:\n{context}");

        vec![
            Message::from_legacy(Role::System, system),
            Message::from_legacy(Role::User, user),
        ]
    }

    fn parse_verdict(response: &str) -> ProbeVerdict {
        // Try to extract JSON from the response.
        let start = response.find('{');
        let end = response.rfind('}');
        if let (Some(s), Some(e)) = (start, end)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&response[s..=e])
        {
            match v.get("verdict").and_then(|x| x.as_str()) {
                Some("allow") => return ProbeVerdict::Allow,
                Some("deny") => {
                    let reason = v
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("safety probe denied this tool call")
                        .to_owned();
                    return ProbeVerdict::Deny { reason };
                }
                _ => {}
            }
        }
        // Unparseable response → allow (fail-open)
        tracing::warn!(
            raw = %response,
            "ShadowSentinel: probe response could not be parsed, defaulting to Allow"
        );
        ProbeVerdict::Allow
    }
}

impl SafetyProbe for LlmSafetyProbe {
    fn evaluate<'a>(
        &'a self,
        tool_id: &'a str,
        tool_args: &'a JsonValue,
        trajectory: &'a [SentinelEvent],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeVerdict> + Send + 'a>> {
        let span = info_span!("security.shadow.probe", tool_id = %tool_id);
        Box::pin(
            async move {
                let messages = Self::build_prompt(tool_id, tool_args, trajectory);
                let timeout = std::time::Duration::from_millis(self.timeout_ms);

                match tokio::time::timeout(timeout, self.provider.chat(&messages)).await {
                    Ok(Ok(response)) => Self::parse_verdict(&response),
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "ShadowSentinel: probe LLM error");
                        if self.deny_on_timeout {
                            ProbeVerdict::Deny {
                                reason: format!("probe LLM error: {e}"),
                            }
                        } else {
                            ProbeVerdict::Allow
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            timeout_ms = self.timeout_ms,
                            "ShadowSentinel: probe timed out"
                        );
                        if self.deny_on_timeout {
                            ProbeVerdict::Deny {
                                reason: "safety probe timed out".to_owned(),
                            }
                        } else {
                            ProbeVerdict::Allow
                        }
                    }
                }
            }
            .instrument(span),
        )
    }
}

// ── ShadowEventStore ─────────────────────────────────────────────────────────

/// Persistent storage for the safety shadow event stream.
///
/// Thin wrapper around [`DbPool`] for the `safety_shadow_events` table.
/// Methods are `async` and return typed errors.
#[derive(Clone)]
pub struct ShadowEventStore {
    pool: DbPool,
}

impl ShadowEventStore {
    /// Create a `ShadowEventStore` backed by the given pool.
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Persist a shadow event to the database.
    ///
    /// The `id` field of the event is ignored; the database assigns a new row id.
    ///
    /// # Errors
    ///
    /// Returns `AgentError` on database failure.
    #[tracing::instrument(name = "security.shadow.record", skip_all, fields(event_type = %event.event_type))]
    pub async fn record(&self, event: &SentinelEvent) -> Result<(), AgentError> {
        zeph_db::query(sql!(
            "INSERT INTO safety_shadow_events \
             (session_id, turn_number, event_type, tool_id, risk_signal, risk_level, \
              probe_verdict, context_summary, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(event.session_id.as_str())
        .bind(i64::try_from(event.turn_number).unwrap_or(i64::MAX))
        .bind(&event.event_type)
        .bind(&event.tool_id)
        .bind(&event.risk_signal)
        .bind(&event.risk_level)
        .bind(&event.probe_verdict)
        .bind(&event.context_summary)
        .bind(event.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| AgentError::Db(e.into()))?;

        Ok(())
    }

    /// Retrieve the last `limit` events for a session in ascending time order.
    ///
    /// Used to build the trajectory context for probe evaluation.
    ///
    /// # Errors
    ///
    /// Returns `AgentError` on database failure.
    #[tracing::instrument(name = "security.shadow.get_trajectory", skip(self), fields(session_id = %session_id))]
    pub async fn get_trajectory(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<SentinelEvent>, AgentError> {
        let rows = zeph_db::query_as::<_, ShadowEventRow>(sql!(
            "SELECT id, session_id, turn_number, event_type, tool_id, risk_signal, \
             risk_level, probe_verdict, context_summary, created_at \
             FROM safety_shadow_events \
             WHERE session_id = ? \
             ORDER BY created_at DESC \
             LIMIT ?"
        ))
        .bind(session_id)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AgentError::Db(e.into()))?;

        // DB returns DESC (newest first); reverse once to get ASC (oldest first) for LLM context.
        let mut events: Vec<SentinelEvent> = rows.into_iter().map(SentinelEvent::from).collect();
        events.reverse();
        Ok(events)
    }

    /// Retrieve the last `limit` events for a specific tool from sessions OTHER than
    /// `exclude_session_id`.
    ///
    /// Used for cross-session pattern detection. The exclusion is applied in SQL (not just
    /// filtered client-side afterward) so that a session with heavy recent activity for
    /// `tool_id` cannot crowd its own rows into the `LIMIT` clip and starve genuinely
    /// cross-session rows out of the result.
    ///
    /// # Errors
    ///
    /// Returns `AgentError` on database failure.
    #[tracing::instrument(name = "security.shadow.get_tool_history", skip(self), fields(tool_id = %tool_id))]
    pub async fn get_tool_history(
        &self,
        tool_id: &str,
        exclude_session_id: &str,
        limit: usize,
    ) -> Result<Vec<SentinelEvent>, AgentError> {
        let rows = zeph_db::query_as::<_, ShadowEventRow>(sql!(
            "SELECT id, session_id, turn_number, event_type, tool_id, risk_signal, \
             risk_level, probe_verdict, context_summary, created_at \
             FROM safety_shadow_events \
             WHERE tool_id = ? AND session_id != ? \
             ORDER BY created_at DESC \
             LIMIT ?"
        ))
        .bind(tool_id)
        .bind(exclude_session_id)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AgentError::Db(e.into()))?;

        Ok(rows.into_iter().map(SentinelEvent::from).collect())
    }
}

// Internal sqlx row type for `safety_shadow_events`.
#[derive(sqlx::FromRow)]
struct ShadowEventRow {
    id: i64,
    session_id: String,
    turn_number: i64,
    event_type: String,
    tool_id: Option<String>,
    risk_signal: Option<String>,
    risk_level: String,
    probe_verdict: Option<String>,
    context_summary: Option<String>,
    created_at: i64,
}

impl From<ShadowEventRow> for SentinelEvent {
    fn from(r: ShadowEventRow) -> Self {
        Self {
            id: r.id,
            session_id: SessionId::new(r.session_id),
            turn_number: u64::try_from(r.turn_number).unwrap_or(0),
            event_type: r.event_type,
            tool_id: r.tool_id,
            risk_signal: r.risk_signal,
            risk_level: r.risk_level,
            probe_verdict: r.probe_verdict,
            context_summary: r.context_summary,
            created_at: r.created_at,
        }
    }
}

// ── ShadowSentinel ────────────────────────────────────────────────────────────

/// Maximum number of concurrent fire-and-forget persist tasks tracked in `pending_writes`.
///
/// When the set is at capacity the oldest completed tasks are reaped before spawning a new one.
/// If the set is still full after reaping (all tasks are still running), the new spawn is skipped
/// with a debug log — persistence is best-effort and the sentinel must never block tool dispatch.
const MAX_PENDING_WRITES: usize = 32;

/// Orchestrates the persistent safety stream and LLM pre-execution probe.
///
/// `ShadowSentinel` is wrapped in `Arc` and shared between `ShadowProbeExecutor` instances
/// when tools run in parallel. All mutable state uses `AtomicU32` to allow `&self` access
/// from concurrent tool dispatch without a `Mutex`.
///
/// # Turn lifecycle
///
/// - `advance_turn()` — call once per turn before tool execution; resets the per-turn
///   probe counter.
/// - `check_tool_call()` — call before each tool execution to probe high-risk calls.
/// - `record_tool_event()` — call after tool execution to persist the event.
/// - `drain_pending()` — call at session shutdown to await all queued persist writes.
///
/// # NEVER
///
/// Never expose the `ShadowSentinel` state or probe verdicts to LLM-visible context.
pub struct ShadowSentinel {
    store: ShadowEventStore,
    probe: Box<dyn SafetyProbe>,
    config: zeph_config::ShadowSentinelConfig,
    /// Counter of probe calls made in the current turn. Uses `AtomicU32` so all
    /// probe-checking methods can take `&self` even under parallel tool execution.
    probes_this_turn: AtomicU32,
    session_id: SessionId,
    /// Bounded set of fire-and-forget DB persist tasks. Prevents unbounded task accumulation
    /// and ensures panics surface at `drain_pending()` instead of being silently swallowed.
    pending_writes: Mutex<JoinSet<()>>,
    /// Sanitized ids (`ToolDef::server_id`-backed) of tools registered by MCP servers.
    ///
    /// Mirrors `TrustGateExecutor::mcp_tool_ids` (`zeph_tools::TrustGateExecutor`): empty until
    /// populated post-construction via [`mcp_tool_ids_handle`](Self::mcp_tool_ids_handle). This
    /// is the authoritative way to know a `qualified_tool_id` originates from an MCP server —
    /// real ids are `{server_id}_{name}` (`McpTool::sanitized_id`) and carry no reliable string
    /// prefix to pattern-match on (#5736).
    ///
    /// # Refresh
    ///
    /// Populated at startup from the initial `mcp_tools` list (`src/runner.rs`) and refreshed
    /// on every subsequent tool-list change — `/mcp add`/`/mcp remove` and a live
    /// `tools/list_changed` notification both route through
    /// `Agent::refresh_shadow_sentinel_mcp_tool_ids` (`crates/zeph-core/src/agent/mcp.rs`,
    /// called from `check_tool_refresh` once per turn) — so a server connected mid-session is
    /// reflected without a process restart.
    ///
    /// **Known gap**: `TrustGateExecutor`'s own equivalent set has no such refresh path (it
    /// lives entirely in the binary crate's tool-executor chain, unreachable from `Agent`) —
    /// tracked separately (#5747), not fixed by this refresh.
    mcp_tool_ids: Arc<RwLock<HashSet<String>>>,
}

impl ShadowSentinel {
    /// Create a new `ShadowSentinel`.
    ///
    /// # Arguments
    ///
    /// * `store` — persistent shadow event store.
    /// * `probe` — safety probe implementation.
    /// * `config` — subsystem configuration.
    /// * `session_id` — current agent session identifier.
    #[must_use]
    pub fn new(
        store: ShadowEventStore,
        probe: Box<dyn SafetyProbe>,
        config: zeph_config::ShadowSentinelConfig,
        session_id: impl Into<SessionId>,
    ) -> Self {
        Self {
            store,
            probe,
            config,
            probes_this_turn: AtomicU32::new(0),
            session_id: session_id.into(),
            pending_writes: Mutex::new(JoinSet::new()),
            mcp_tool_ids: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Returns the shared MCP tool-id set so the caller can populate it once MCP servers have
    /// connected (mirrors `TrustGateExecutor::mcp_tool_ids_handle`).
    #[must_use]
    pub fn mcp_tool_ids_handle(&self) -> Arc<RwLock<HashSet<String>>> {
        Arc::clone(&self.mcp_tool_ids)
    }

    /// Returns `true` when `tool_id` was registered by an MCP server.
    fn is_mcp_tool(&self, tool_id: &str) -> bool {
        self.mcp_tool_ids.read().contains(tool_id)
    }

    /// Classify a fully-qualified tool id into a risk tier.
    ///
    /// Pattern matching is prefix/glob-based against the configured `probe_patterns`.
    /// For efficiency, we check common built-in names first before falling back to
    /// glob matching against the configured patterns.
    #[must_use]
    pub fn classify_tool(&self, qualified_tool_id: &str) -> ToolRiskCategory {
        // Fast-path for well-known high-risk builtins.
        if qualified_tool_id == "builtin:shell"
            || qualified_tool_id == "builtin:bash"
            || qualified_tool_id.starts_with("builtin:shell")
            || qualified_tool_id == "bash"
            || qualified_tool_id == "shell"
            || qualified_tool_id == "sh"
        {
            return ToolRiskCategory::Shell;
        }
        if qualified_tool_id == "builtin:write"
            || qualified_tool_id == "builtin:edit"
            || qualified_tool_id == "builtin:delete"
            || qualified_tool_id == "write"
            || qualified_tool_id == "edit"
            || qualified_tool_id == "delete"
        {
            return ToolRiskCategory::FileWrite;
        }

        // Glob matching against configured patterns.
        for pattern in &self.config.probe_patterns {
            if glob_matches(pattern, qualified_tool_id) {
                // Classify based on the pattern name.
                if pattern.contains("shell") || pattern.contains("exec") {
                    return ToolRiskCategory::Shell;
                }
                if pattern.contains("write")
                    || pattern.contains("edit")
                    || pattern.contains("delete")
                    || pattern.contains("file")
                {
                    if self.is_mcp_tool(qualified_tool_id) {
                        return ToolRiskCategory::ExfilCapable;
                    }
                    return ToolRiskCategory::FileWrite;
                }
                return ToolRiskCategory::ExfilCapable;
            }
        }

        ToolRiskCategory::Low
    }

    /// Evaluate a proposed tool call and return a probe verdict.
    ///
    /// Returns `ProbeVerdict::Skip` when:
    /// - The tool is not in a high-risk category.
    /// - The feature is disabled.
    /// - The per-turn probe budget (`max_probes_per_turn`) is exhausted.
    ///
    /// This method takes `&self` so it can be called from parallel tool dispatch.
    ///
    /// # Errors
    ///
    /// Does not return errors; probe failures are handled internally (fail-open or
    /// fail-closed depending on `deny_on_timeout`).
    #[tracing::instrument(name = "security.shadow.check", skip(self, tool_args), fields(tool_id = %qualified_tool_id))]
    pub async fn check_tool_call(
        &self,
        qualified_tool_id: &str,
        tool_args: &JsonValue,
        turn_number: u64,
        current_risk_level: &str,
    ) -> ProbeVerdict {
        if !self.config.enabled {
            return ProbeVerdict::Skip;
        }

        let category = self.classify_tool(qualified_tool_id);
        if category == ToolRiskCategory::Low {
            return ProbeVerdict::Skip;
        }

        // Check per-turn probe budget using relaxed atomics (false sharing is acceptable here).
        let count = self.probes_this_turn.fetch_add(1, Ordering::Relaxed);
        let max_probes = u32::try_from(self.config.max_probes_per_turn).unwrap_or(u32::MAX);
        if count >= max_probes {
            // Undo the increment so future fast-path checks are accurate.
            self.probes_this_turn.fetch_sub(1, Ordering::Relaxed);
            tracing::debug!(
                max = self.config.max_probes_per_turn,
                "ShadowSentinel: probe budget exhausted for this turn, skipping"
            );
            return ProbeVerdict::Skip;
        }

        // Load recent trajectory for probe context.
        // Filter out probe_result events — exposing probe verdicts to the LLM would allow
        // prompt injection attacks that craft tool outputs to manipulate perceived safety.
        let mut trajectory: Vec<SentinelEvent> = match self
            .store
            .get_trajectory(&self.session_id, self.config.max_context_events)
            .await
        {
            Ok(t) => t
                .into_iter()
                .filter(|e| e.event_type != "probe_result")
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "ShadowSentinel: failed to load trajectory, proceeding without context");
                vec![]
            }
        };

        // Reserve half the total budget for cross-session history so recurring risk patterns
        // from other sessions always have visibility — even in the busiest sessions, where the
        // session's own trajectory alone would otherwise fill (and, pre-fix, silently evict
        // the entire cross-session block from) the whole budget. Enforce the session-side cap
        // here (trajectory is oldest-first/ASC, so excess is trimmed from the front, keeping
        // the most recent events).
        let cross_session_budget = self.config.max_context_events / 2;
        let session_budget = self.config.max_context_events - cross_session_budget;
        if trajectory.len() > session_budget {
            let excess = trajectory.len() - session_budget;
            trajectory.drain(0..excess);
        }

        // Load cross-session history for this tool so recurring risk patterns from
        // other sessions inform the probe, not just the current session (#5449). The
        // current session is excluded in SQL (not just filtered client-side) so its own
        // activity can never crowd genuinely cross-session rows out of the LIMIT clip.
        match self
            .store
            .get_tool_history(
                qualified_tool_id,
                self.session_id.as_str(),
                self.config.max_context_events,
            )
            .await
        {
            Ok(history) => {
                // get_tool_history is DESC (newest first); reverse to ASC to match
                // trajectory ordering, then prepend so trajectory stays oldest-first.
                let mut cross_session: Vec<SentinelEvent> = history
                    .into_iter()
                    .filter(|e| e.event_type != "probe_result")
                    .rev()
                    .collect();
                if cross_session.len() > cross_session_budget {
                    let excess = cross_session.len() - cross_session_budget;
                    cross_session.drain(0..excess);
                }
                trajectory.splice(0..0, cross_session);
            }
            Err(e) => {
                tracing::warn!(error = %e, "ShadowSentinel: failed to load cross-session tool history, proceeding without it");
            }
        }

        let verdict = self
            .probe
            .evaluate(qualified_tool_id, tool_args, &trajectory)
            .await;

        // Persist the probe result asynchronously (best-effort — never blocks tool path).
        let probe_verdict_str = match &verdict {
            ProbeVerdict::Allow => "allow",
            ProbeVerdict::Deny { .. } => "deny",
            ProbeVerdict::Skip => "skip",
        };
        let summary = match &verdict {
            ProbeVerdict::Deny { reason } => {
                format!("probe denied: {}", &reason[..reason.len().min(120)])
            }
            ProbeVerdict::Allow => format!("probe allowed {qualified_tool_id}"),
            ProbeVerdict::Skip => format!("probe skipped {qualified_tool_id}"),
        };
        let event = SentinelEvent {
            id: 0,
            session_id: self.session_id.clone(),
            turn_number,
            event_type: "probe_result".to_owned(),
            tool_id: Some(qualified_tool_id.to_owned()),
            risk_signal: None,
            risk_level: current_risk_level.to_owned(),
            probe_verdict: Some(probe_verdict_str.to_owned()),
            context_summary: Some(summary),
            created_at: unix_now(),
        };
        let store = self.store.clone();
        self.spawn_persist(async move {
            if let Err(e) = store.record(&event).await {
                tracing::warn!(error = %e, "ShadowSentinel: failed to persist probe result");
            }
        })
        .await;

        verdict
    }

    /// Persist a tool execution event in the shadow stream (fire-and-forget).
    ///
    /// Called after a tool finishes execution to maintain the trajectory for future probes.
    pub async fn record_tool_event(
        &self,
        qualified_tool_id: &str,
        turn_number: u64,
        risk_level: &str,
        context_summary: &str,
    ) {
        if !self.config.enabled {
            return;
        }
        let event = SentinelEvent {
            id: 0,
            session_id: self.session_id.clone(),
            turn_number,
            event_type: "tool_call".to_owned(),
            tool_id: Some(qualified_tool_id.to_owned()),
            risk_signal: None,
            risk_level: risk_level.to_owned(),
            probe_verdict: None,
            context_summary: Some(context_summary.chars().take(250).collect()),
            created_at: unix_now(),
        };
        let store = self.store.clone();
        self.spawn_persist(async move {
            if let Err(e) = store.record(&event).await {
                tracing::warn!(error = %e, "ShadowSentinel: failed to persist tool event");
            }
        })
        .await;
    }

    /// Await all queued fire-and-forget persist tasks.
    ///
    /// Call once at session shutdown to ensure no DB writes are silently dropped.
    /// All errors have already been logged inside each task; this method only joins the handles.
    pub async fn drain_pending(&self) {
        let mut set = {
            let mut guard = self.pending_writes.lock().await;
            std::mem::take(&mut *guard)
        };
        while set.join_next().await.is_some() {}
    }

    /// Spawn a background persist task into the bounded `JoinSet`.
    ///
    /// Reaps completed handles before spawning to stay within `MAX_PENDING_WRITES`. If the set
    /// is still at capacity after reaping (all tasks still running), the new task is dropped and
    /// a debug message is emitted — persistence is best-effort and must never block the tool path.
    async fn spawn_persist<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut set = self.pending_writes.lock().await;
        // Reap only already-finished handles — never block waiting for a running task.
        // try_join_next() returns immediately if no task has completed yet.
        while set.try_join_next().is_some() {}
        if set.len() < MAX_PENDING_WRITES {
            set.spawn(fut);
        } else {
            tracing::debug!(
                max = MAX_PENDING_WRITES,
                "ShadowSentinel: pending_writes at capacity, skipping persist"
            );
        }
    }

    /// Reset the per-turn probe counter.
    ///
    /// Must be called once per turn BEFORE any tool calls, alongside
    /// `TrajectorySentinel::advance_turn()`.
    pub fn advance_turn(&self) {
        self.probes_this_turn.store(0, Ordering::Release);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns the current Unix timestamp in seconds.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// Simple glob matching: `*` matches any sequence of characters except `/`.
/// `*/` in the pattern matches any single path segment.
fn glob_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    // Split on `*` and check each segment is present in order.
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == value;
    }
    let mut remaining = value;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !remaining.starts_with(part) {
                return false;
            }
            remaining = &remaining[part.len()..];
        } else if i == parts.len() - 1 {
            return remaining.ends_with(part);
        } else if let Some(pos) = remaining.find(part) {
            remaining = &remaining[pos + part.len()..];
        } else {
            return false;
        }
    }
    true
}

// ── AgentError extension ──────────────────────────────────────────────────────
// ShadowEventStore uses AgentError::Db — add that variant if missing.
// (The actual variant is declared in agent/error.rs; we only reference it here.)

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn classify_builtin_shell_is_shell_risk() {
        let config = zeph_config::ShadowSentinelConfig::default();
        let sentinel = make_test_sentinel(config).await;
        assert_eq!(
            sentinel.classify_tool("builtin:shell"),
            ToolRiskCategory::Shell
        );
        assert_eq!(
            sentinel.classify_tool("builtin:bash"),
            ToolRiskCategory::Shell
        );
    }

    #[tokio::test]
    async fn classify_builtin_write_is_file_write_risk() {
        let config = zeph_config::ShadowSentinelConfig::default();
        let sentinel = make_test_sentinel(config).await;
        assert_eq!(
            sentinel.classify_tool("builtin:write"),
            ToolRiskCategory::FileWrite
        );
        assert_eq!(
            sentinel.classify_tool("builtin:edit"),
            ToolRiskCategory::FileWrite
        );
    }

    #[tokio::test]
    async fn classify_low_risk_returns_low() {
        let config = zeph_config::ShadowSentinelConfig::default();
        let sentinel = make_test_sentinel(config).await;
        assert_eq!(
            sentinel.classify_tool("builtin:read"),
            ToolRiskCategory::Low
        );
        assert_eq!(
            sentinel.classify_tool("builtin:search"),
            ToolRiskCategory::Low
        );
    }

    #[tokio::test]
    async fn classify_bare_shell_names_are_shell_risk() {
        let config = zeph_config::ShadowSentinelConfig::default();
        let sentinel = make_test_sentinel(config).await;
        assert_eq!(sentinel.classify_tool("bash"), ToolRiskCategory::Shell);
        assert_eq!(sentinel.classify_tool("shell"), ToolRiskCategory::Shell);
        assert_eq!(sentinel.classify_tool("sh"), ToolRiskCategory::Shell);
    }

    #[tokio::test]
    async fn classify_bare_file_write_names_are_file_write_risk() {
        let config = zeph_config::ShadowSentinelConfig::default();
        let sentinel = make_test_sentinel(config).await;
        assert_eq!(sentinel.classify_tool("write"), ToolRiskCategory::FileWrite);
        assert_eq!(sentinel.classify_tool("edit"), ToolRiskCategory::FileWrite);
        assert_eq!(
            sentinel.classify_tool("delete"),
            ToolRiskCategory::FileWrite
        );
    }

    /// #5736 regression: MCP-tool escalation must key off the registered `mcp_tool_ids` set
    /// (`ToolDef::server_id`-backed), not a `"mcp:"` string prefix — real MCP tool ids are
    /// `"{server_id}_{name}"` (`McpTool::sanitized_id`) and never carry that prefix, so the old
    /// check silently never escalated any MCP write/edit tool to `ExfilCapable`.
    #[tokio::test]
    async fn classify_mcp_tool_write_pattern_escalates_to_exfil_capable() {
        let config = zeph_config::ShadowSentinelConfig {
            probe_patterns: vec!["*edit*".to_owned()],
            ..zeph_config::ShadowSentinelConfig::default()
        };
        let sentinel = make_test_sentinel(config).await;
        // Unregistered: a same-shaped id falls to the ordinary FileWrite tier, not ExfilCapable.
        assert_eq!(
            sentinel.classify_tool("github_edit_file"),
            ToolRiskCategory::FileWrite
        );
        // Register it the same way the real MCP wiring does (via the shared handle) and the
        // identical id must now escalate.
        sentinel
            .mcp_tool_ids_handle()
            .write()
            .insert("github_edit_file".to_owned());
        assert_eq!(
            sentinel.classify_tool("github_edit_file"),
            ToolRiskCategory::ExfilCapable
        );
    }

    /// #5736 follow-up (CI-1239): `ShadowSentinelConfig::default()` — not a hand-tuned override —
    /// must escalate a real MCP write tool. Real MCP tool ids are `"{server_id}_{name}"`
    /// (`McpTool::sanitized_id`), e.g. `"fs-test_write_file"`; the shipped default
    /// `probe_patterns` (`"mcp:*/file_*"`, `"mcp:*/exec_*"`) assumed a `"mcp:"`-prefixed id
    /// shape that no real id ever has, so the outer glob-matching loop in `classify_tool` never
    /// even entered the branch containing the `is_mcp_tool()` check — every MCP tool silently
    /// fell through to `ToolRiskCategory::Low` (probe skipped entirely), a complete bypass, not
    /// just a downgrade to `FileWrite`.
    #[tokio::test]
    async fn classify_mcp_tool_write_under_default_config_escalates_to_exfil_capable() {
        let config = zeph_config::ShadowSentinelConfig::default();
        let sentinel = make_test_sentinel(config).await;
        sentinel
            .mcp_tool_ids_handle()
            .write()
            .insert("fs-test_write_file".to_owned());
        assert_eq!(
            sentinel.classify_tool("fs-test_write_file"),
            ToolRiskCategory::ExfilCapable
        );
    }

    #[tokio::test]
    async fn advance_turn_resets_counter() {
        let config = zeph_config::ShadowSentinelConfig::default();
        let sentinel = make_test_sentinel(config).await;
        sentinel.probes_this_turn.store(3, Ordering::Relaxed);
        sentinel.advance_turn();
        assert_eq!(sentinel.probes_this_turn.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn glob_matches_star_wildcard() {
        assert!(glob_matches("mcp:*/file_*", "mcp:myserver/file_read"));
        assert!(glob_matches("mcp:*/file_*", "mcp:other/file_write"));
        assert!(!glob_matches("mcp:*/file_*", "builtin:shell"));
    }

    #[test]
    fn glob_matches_exact() {
        assert!(glob_matches("builtin:shell", "builtin:shell"));
        assert!(!glob_matches("builtin:shell", "builtin:write"));
    }

    #[test]
    fn parse_verdict_allow() {
        let v = LlmSafetyProbe::parse_verdict(r#"{"verdict": "allow"}"#);
        assert_eq!(v, ProbeVerdict::Allow);
    }

    #[test]
    fn parse_verdict_deny_with_reason() {
        let v =
            LlmSafetyProbe::parse_verdict(r#"{"verdict": "deny", "reason": "suspicious pattern"}"#);
        assert_eq!(
            v,
            ProbeVerdict::Deny {
                reason: "suspicious pattern".to_owned()
            }
        );
    }

    #[test]
    fn parse_verdict_unparseable_allows() {
        let v = LlmSafetyProbe::parse_verdict("I think this is fine");
        assert_eq!(v, ProbeVerdict::Allow);
    }

    #[tokio::test]
    async fn check_tool_call_skips_after_budget_exhausted() {
        let config = zeph_config::ShadowSentinelConfig {
            enabled: true,
            max_probes_per_turn: 2,
            ..zeph_config::ShadowSentinelConfig::default()
        };
        let sentinel = make_test_sentinel(config).await;

        // First two calls should not be skipped (noop probe returns Allow).
        let args = serde_json::Value::Object(serde_json::Map::new());
        let v1 = sentinel
            .check_tool_call("builtin:shell", &args, 1, "calm")
            .await;
        let v2 = sentinel
            .check_tool_call("builtin:shell", &args, 1, "calm")
            .await;
        assert_ne!(v1, ProbeVerdict::Skip, "first call within budget");
        assert_ne!(v2, ProbeVerdict::Skip, "second call within budget");

        // Third call exceeds max_probes_per_turn = 2 → must skip.
        let v3 = sentinel
            .check_tool_call("builtin:shell", &args, 1, "calm")
            .await;
        assert_eq!(
            v3,
            ProbeVerdict::Skip,
            "third call must be skipped (budget exhausted)"
        );
    }

    #[tokio::test]
    async fn check_tool_call_returns_skip_when_disabled() {
        let config = zeph_config::ShadowSentinelConfig {
            enabled: false,
            ..zeph_config::ShadowSentinelConfig::default()
        };
        let sentinel = make_test_sentinel(config).await;
        let args = serde_json::Value::Object(serde_json::Map::new());
        let verdict = sentinel
            .check_tool_call("builtin:shell", &args, 1, "calm")
            .await;
        assert_eq!(
            verdict,
            ProbeVerdict::Skip,
            "disabled sentinel must always return Skip without calling the probe"
        );
    }

    // ── JoinSet regression tests (#4570) ─────────────────────────────────────

    /// `drain_pending` awaits all spawned persist tasks and returns when the set is empty.
    #[tokio::test]
    async fn drain_pending_awaits_all_tasks() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let config = zeph_config::ShadowSentinelConfig::default();
        let sentinel = make_test_sentinel(config).await;

        let counter = Arc::new(AtomicU32::new(0));
        for _ in 0..5 {
            let c = Arc::clone(&counter);
            sentinel
                .spawn_persist(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    c.fetch_add(1, Ordering::Relaxed);
                })
                .await;
        }

        sentinel.drain_pending().await;

        assert_eq!(
            counter.load(Ordering::Relaxed),
            5,
            "drain_pending must join all 5 tasks before returning"
        );
    }

    /// When the pending set is at capacity and all running tasks complete before the next
    /// `spawn_persist`, the new task IS accepted (the set has room after reaping).
    /// Conversely, if we fill the set, drain it, then overfill past capacity while tasks are
    /// still running — the implementation drops extras.  We verify the simpler property:
    /// `spawn_persist` never panics when called repeatedly beyond `MAX_PENDING_WRITES`.
    #[tokio::test]
    async fn spawn_persist_beyond_capacity_does_not_panic() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let config = zeph_config::ShadowSentinelConfig::default();
        let sentinel = make_test_sentinel(config).await;
        let counter = Arc::new(AtomicU32::new(0));

        // Spawn twice the capacity; each task completes instantly.
        // spawn_persist will reap completed tasks between spawns, so most will be accepted.
        for _ in 0..(MAX_PENDING_WRITES * 2) {
            let c = Arc::clone(&counter);
            sentinel
                .spawn_persist(async move {
                    c.fetch_add(1, Ordering::Relaxed);
                })
                .await;
        }

        sentinel.drain_pending().await;

        // All tasks (or at least MAX_PENDING_WRITES of them) must have run; none panicked.
        let ran = counter.load(Ordering::Relaxed);
        assert!(
            ran >= u32::try_from(MAX_PENDING_WRITES).unwrap(),
            "at least MAX_PENDING_WRITES tasks must complete; ran={ran}"
        );
    }

    // Build a minimal ShadowSentinel with a no-op probe for unit tests.
    //
    // Opens an in-memory SQLite pool. Store methods are never called in these unit
    // tests — they test only classification and counter logic.
    async fn make_test_sentinel(config: zeph_config::ShadowSentinelConfig) -> ShadowSentinel {
        struct NoopProbe;
        impl SafetyProbe for NoopProbe {
            fn evaluate<'a>(
                &'a self,
                _: &'a str,
                _: &'a JsonValue,
                _: &'a [SentinelEvent],
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeVerdict> + Send + 'a>>
            {
                Box::pin(async { ProbeVerdict::Allow })
            }
        }
        let pool = test_pool().await;
        let store = ShadowEventStore::new(pool);
        ShadowSentinel::new(store, Box::new(NoopProbe), config, "test-session")
    }

    // Opens a migrated in-memory SQLite pool (unlike `make_test_sentinel`'s pool, this one
    // has the `safety_shadow_events` table from migration 085 and can serve real store queries.
    async fn test_pool() -> DbPool {
        zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        }
        .connect()
        .await
        .expect("connect + migrate in-memory sqlite pool")
    }

    fn make_event(
        session_id: &str,
        turn_number: u64,
        tool_id: &str,
        summary: &str,
    ) -> SentinelEvent {
        SentinelEvent {
            id: 0,
            session_id: SessionId::new(session_id),
            turn_number,
            event_type: "tool_call".to_owned(),
            tool_id: Some(tool_id.to_owned()),
            risk_signal: None,
            risk_level: "elevated".to_owned(),
            probe_verdict: None,
            context_summary: Some(summary.to_owned()),
            created_at: unix_now(),
        }
    }

    #[tokio::test]
    async fn get_tool_history_returns_events_across_sessions() {
        let store = ShadowEventStore::new(test_pool().await);

        store
            .record(&make_event(
                "session-a",
                1,
                "builtin:shell",
                "session-a ran a command",
            ))
            .await
            .expect("record session-a event");
        store
            .record(&make_event(
                "session-b",
                1,
                "builtin:shell",
                "session-b ran a command",
            ))
            .await
            .expect("record session-b event");
        store
            .record(&make_event(
                "session-a",
                2,
                "builtin:write",
                "unrelated tool",
            ))
            .await
            .expect("record unrelated-tool event");

        let history = store
            .get_tool_history("builtin:shell", "unrelated-session", 10)
            .await
            .expect("get_tool_history");

        assert_eq!(
            history.len(),
            2,
            "must return events from both non-excluded sessions for the queried tool_id, \
             excluding other tools"
        );
        assert!(history.iter().any(|e| e.session_id.as_str() == "session-a"));
        assert!(history.iter().any(|e| e.session_id.as_str() == "session-b"));

        let history_excluding_a = store
            .get_tool_history("builtin:shell", "session-a", 10)
            .await
            .expect("get_tool_history");
        assert_eq!(
            history_excluding_a.len(),
            1,
            "exclude_session_id must be applied in SQL, not just usable for client-side \
             filtering afterward"
        );
        assert!(
            history_excluding_a
                .iter()
                .all(|e| e.session_id.as_str() != "session-a")
        );
    }

    /// #5449 regression: `check_tool_call` must fold cross-session `get_tool_history` results
    /// into the trajectory passed to the probe, not just the current session's own events.
    #[tokio::test]
    async fn check_tool_call_incorporates_cross_session_tool_history() {
        struct CapturingProbe {
            captured: Arc<Mutex<Vec<SentinelEvent>>>,
        }
        impl SafetyProbe for CapturingProbe {
            fn evaluate<'a>(
                &'a self,
                _tool_id: &'a str,
                _tool_args: &'a JsonValue,
                trajectory: &'a [SentinelEvent],
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeVerdict> + Send + 'a>>
            {
                let captured = Arc::clone(&self.captured);
                let trajectory = trajectory.to_vec();
                Box::pin(async move {
                    *captured.lock().await = trajectory;
                    ProbeVerdict::Allow
                })
            }
        }

        let store = ShadowEventStore::new(test_pool().await);
        let other_session = "other-session";
        store
            .record(&make_event(
                other_session,
                1,
                "builtin:shell",
                "other session ran rm -rf",
            ))
            .await
            .expect("record cross-session event");

        let captured: Arc<Mutex<Vec<SentinelEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let config = zeph_config::ShadowSentinelConfig {
            enabled: true,
            ..zeph_config::ShadowSentinelConfig::default()
        };
        let sentinel = ShadowSentinel::new(
            store,
            Box::new(CapturingProbe {
                captured: Arc::clone(&captured),
            }),
            config,
            "current-session",
        );

        let args = serde_json::Value::Object(serde_json::Map::new());
        sentinel
            .check_tool_call("builtin:shell", &args, 1, "calm")
            .await;

        let seen = captured.lock().await;
        assert!(
            seen.iter().any(|e| e.session_id.as_str() == other_session
                && e.context_summary.as_deref() == Some("other session ran rm -rf")),
            "probe context must include the cross-session tool history event, got: {seen:?}"
        );
    }

    /// Drives `check_tool_call` for `tool_id` under `session_id` and returns the exact
    /// trajectory the probe received, so cap tests can assert WHICH events survive, not
    /// just how many — a count-only assertion can pass while the cap silently drops all
    /// cross-session data (the bug found in code review of the initial #5449 fix).
    async fn capture_check_tool_call_trajectory(
        store: ShadowEventStore,
        config: zeph_config::ShadowSentinelConfig,
        session_id: &str,
        tool_id: &str,
    ) -> Vec<SentinelEvent> {
        struct CapturingProbe {
            captured: Arc<Mutex<Vec<SentinelEvent>>>,
        }
        impl SafetyProbe for CapturingProbe {
            fn evaluate<'a>(
                &'a self,
                _tool_id: &'a str,
                _tool_args: &'a JsonValue,
                trajectory: &'a [SentinelEvent],
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeVerdict> + Send + 'a>>
            {
                let captured = Arc::clone(&self.captured);
                let trajectory = trajectory.to_vec();
                Box::pin(async move {
                    *captured.lock().await = trajectory;
                    ProbeVerdict::Allow
                })
            }
        }

        let captured: Arc<Mutex<Vec<SentinelEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sentinel = ShadowSentinel::new(
            store,
            Box::new(CapturingProbe {
                captured: Arc::clone(&captured),
            }),
            config,
            session_id,
        );
        let args = serde_json::Value::Object(serde_json::Map::new());
        sentinel.check_tool_call(tool_id, &args, 1, "calm").await;
        captured.lock().await.clone()
    }

    /// Seeds `count` events for `session_id`/`tool_id`, with ascending `created_at`
    /// timestamps starting at `base`, so cap tests can control which events are "most
    /// recent". Summaries are `"{summary_prefix}-{i}"` for index-based assertions.
    async fn seed_events(
        store: &ShadowEventStore,
        session_id: &str,
        tool_id: &str,
        summary_prefix: &str,
        base: i64,
        count: u32,
    ) {
        for i in 0..count {
            let mut event = make_event(
                session_id,
                u64::from(i),
                tool_id,
                &format!("{summary_prefix}-{i}"),
            );
            event.created_at = base + i64::from(i);
            store.record(&event).await.expect("record seeded event");
        }
    }

    /// Session trajectory and cross-session history are each independently capped at
    /// `max_context_events`, so a naive merge can total up to 2x the configured budget.
    /// `check_tool_call` must enforce the combined cap AND reserve budget for cross-session
    /// data — the original fix trimmed unconditionally from the front, which silently wiped
    /// ALL cross-session events whenever the session's own trajectory alone filled the
    /// budget (precisely the busiest-session scenario #5449 cares about most).
    #[tokio::test]
    async fn check_tool_call_cap_reserves_cross_session_budget_when_session_heavy() {
        let store = ShadowEventStore::new(test_pool().await);
        let base = unix_now();
        seed_events(
            &store,
            "current-session",
            "builtin:shell",
            "session",
            base,
            4,
        )
        .await;
        seed_events(&store, "other-session", "builtin:shell", "cross", base, 3).await;

        let config = zeph_config::ShadowSentinelConfig {
            enabled: true,
            max_context_events: 4,
            ..zeph_config::ShadowSentinelConfig::default()
        };
        let trajectory =
            capture_check_tool_call_trajectory(store, config, "current-session", "builtin:shell")
                .await;

        assert_eq!(
            trajectory.len(),
            4,
            "total must be capped at max_context_events"
        );
        let cross_session_count = trajectory
            .iter()
            .filter(|e| e.session_id.as_str() == "other-session")
            .count();
        assert_eq!(
            cross_session_count, 2,
            "cross-session budget is max_context_events/2 = 2, and must survive even \
             though the session's own trajectory alone fills the whole budget; \
             got trajectory: {trajectory:?}"
        );
    }

    /// Mirror case: cross-session history is the one over budget, session's own trajectory
    /// is light. The session-side cap must not trim events that don't need trimming.
    #[tokio::test]
    async fn check_tool_call_cap_cross_session_heavy_case() {
        let store = ShadowEventStore::new(test_pool().await);
        let base = unix_now();
        seed_events(
            &store,
            "current-session",
            "builtin:shell",
            "session",
            base,
            1,
        )
        .await;
        seed_events(&store, "other-session", "builtin:shell", "cross", base, 4).await;

        let config = zeph_config::ShadowSentinelConfig {
            enabled: true,
            max_context_events: 4,
            ..zeph_config::ShadowSentinelConfig::default()
        };
        let trajectory =
            capture_check_tool_call_trajectory(store, config, "current-session", "builtin:shell")
                .await;

        let session_count = trajectory
            .iter()
            .filter(|e| e.session_id.as_str() == "current-session")
            .count();
        let cross_session_count = trajectory.len() - session_count;
        assert_eq!(
            session_count, 1,
            "session's own (light) trajectory must not be trimmed"
        );
        assert_eq!(
            cross_session_count, 2,
            "cross-session budget is max_context_events/2 = 2"
        );
    }

    /// Boundary: session + cross-session totals exactly `max_context_events` — nothing
    /// should be dropped from either side.
    #[tokio::test]
    async fn check_tool_call_cap_boundary_at_exact_limit() {
        let store = ShadowEventStore::new(test_pool().await);
        let base = unix_now();
        seed_events(
            &store,
            "current-session",
            "builtin:shell",
            "session",
            base,
            2,
        )
        .await;
        seed_events(&store, "other-session", "builtin:shell", "cross", base, 2).await;

        let config = zeph_config::ShadowSentinelConfig {
            enabled: true,
            max_context_events: 4,
            ..zeph_config::ShadowSentinelConfig::default()
        };
        let trajectory =
            capture_check_tool_call_trajectory(store, config, "current-session", "builtin:shell")
                .await;

        assert_eq!(
            trajectory.len(),
            4,
            "exactly at the limit: nothing should be dropped"
        );
    }

    /// Boundary: one more cross-session event than the reserved budget — exactly one event
    /// must be dropped, and it must be the OLDEST cross-session event (trajectory stays
    /// oldest-first/ASC, so the most recent events are kept).
    #[tokio::test]
    async fn check_tool_call_cap_boundary_at_limit_plus_one() {
        let store = ShadowEventStore::new(test_pool().await);
        let base = unix_now();
        seed_events(
            &store,
            "current-session",
            "builtin:shell",
            "session",
            base,
            2,
        )
        .await;
        seed_events(&store, "other-session", "builtin:shell", "cross", base, 3).await;

        let config = zeph_config::ShadowSentinelConfig {
            enabled: true,
            max_context_events: 4,
            ..zeph_config::ShadowSentinelConfig::default()
        };
        let trajectory =
            capture_check_tool_call_trajectory(store, config, "current-session", "builtin:shell")
                .await;

        assert_eq!(
            trajectory.len(),
            4,
            "limit+1 overall: exactly one event must be dropped"
        );
        let cross_summaries: Vec<&str> = trajectory
            .iter()
            .filter(|e| e.session_id.as_str() == "other-session")
            .filter_map(|e| e.context_summary.as_deref())
            .collect();
        assert_eq!(
            cross_summaries,
            vec!["cross-1", "cross-2"],
            "the oldest cross-session event (cross-0) must be the one dropped, \
             got: {cross_summaries:?}"
        );
    }

    /// The current session's own events must not be double-counted into the cross-session
    /// block — `get_tool_history` excludes `exclude_session_id` directly in its SQL
    /// (`AND session_id != ?`), and this test confirms that exclusion end-to-end through
    /// `check_tool_call`.
    #[tokio::test]
    async fn check_tool_call_excludes_current_session_from_cross_session_merge() {
        let store = ShadowEventStore::new(test_pool().await);
        let base = unix_now();
        seed_events(&store, "current-session", "builtin:shell", "own", base, 2).await;

        let config = zeph_config::ShadowSentinelConfig {
            enabled: true,
            max_context_events: 10,
            ..zeph_config::ShadowSentinelConfig::default()
        };
        let trajectory =
            capture_check_tool_call_trajectory(store, config, "current-session", "builtin:shell")
                .await;

        assert_eq!(
            trajectory.len(),
            2,
            "current session's own events must appear exactly once, not duplicated via \
             the cross-session merge; got: {trajectory:?}"
        );
    }

    /// `probe_result` events from OTHER sessions must never leak into the cross-session
    /// merge — `get_tool_history`'s SQL does not filter by `event_type`, so this relies
    /// entirely on the Rust-side filter (the same LLM-isolation invariant already tested
    /// for the same-session trajectory, exercised here on the cross-session path).
    #[tokio::test]
    async fn check_tool_call_excludes_probe_result_events_from_cross_session_merge() {
        let store = ShadowEventStore::new(test_pool().await);
        let base = unix_now();
        let mut event = make_event("other-session", 1, "builtin:shell", "probe verdict leaked");
        event.event_type = "probe_result".to_owned();
        event.created_at = base;
        store
            .record(&event)
            .await
            .expect("record probe_result event");

        let config = zeph_config::ShadowSentinelConfig {
            enabled: true,
            max_context_events: 10,
            ..zeph_config::ShadowSentinelConfig::default()
        };
        let trajectory =
            capture_check_tool_call_trajectory(store, config, "current-session", "builtin:shell")
                .await;

        assert!(
            trajectory.is_empty(),
            "probe_result events from other sessions must never appear in the \
             cross-session merge (LLM isolation invariant), got: {trajectory:?}"
        );
    }
}
