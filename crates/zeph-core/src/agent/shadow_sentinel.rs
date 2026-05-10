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

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use serde_json::Value as JsonValue;
use tracing::{Instrument as _, info_span};
use zeph_db::DbPool;
use zeph_llm::LlmProvider;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{Message, Role};

use crate::agent::error::AgentError;

// ── Risk category ────────────────────────────────────────────────────────────

/// Classifies a tool into a risk tier for probe gating.
///
/// Only `Shell`, `FileWrite`, and `ExfilCapable` tools trigger a safety probe.
/// `Low` tools bypass the probe entirely, adding zero latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

// ── Shadow event ─────────────────────────────────────────────────────────────

/// A single event in the persistent safety shadow stream.
///
/// Stored in `safety_shadow_events` and retrieved for cross-session probe context.
#[derive(Debug, Clone)]
pub struct ShadowEvent {
    /// Database row id (0 for unsaved records).
    pub id: i64,
    /// Agent session identifier.
    pub session_id: String,
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
        trajectory: &'a [ShadowEvent],
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
        trajectory: &[ShadowEvent],
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
        trajectory: &'a [ShadowEvent],
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
    pub async fn record(&self, event: &ShadowEvent) -> Result<(), AgentError> {
        sqlx::query(
            "INSERT INTO safety_shadow_events \
             (session_id, turn_number, event_type, tool_id, risk_signal, risk_level, \
              probe_verdict, context_summary, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&event.session_id)
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
        .map_err(|e| AgentError::Db(e.to_string()))?;

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
    ) -> Result<Vec<ShadowEvent>, AgentError> {
        let rows = sqlx::query_as::<_, ShadowEventRow>(
            "SELECT id, session_id, turn_number, event_type, tool_id, risk_signal, \
             risk_level, probe_verdict, context_summary, created_at \
             FROM safety_shadow_events \
             WHERE session_id = ? \
             ORDER BY created_at DESC \
             LIMIT ?",
        )
        .bind(session_id)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AgentError::Db(e.to_string()))?;

        // DB returns DESC (newest first); reverse once to get ASC (oldest first) for LLM context.
        let mut events: Vec<ShadowEvent> = rows.into_iter().map(ShadowEvent::from).collect();
        events.reverse();
        Ok(events)
    }

    /// Retrieve the last `limit` events for a specific tool across all sessions.
    ///
    /// Used for cross-session pattern detection.
    ///
    /// # Errors
    ///
    /// Returns `AgentError` on database failure.
    #[tracing::instrument(name = "security.shadow.get_tool_history", skip(self), fields(tool_id = %tool_id))]
    pub async fn get_tool_history(
        &self,
        tool_id: &str,
        limit: usize,
    ) -> Result<Vec<ShadowEvent>, AgentError> {
        let rows = sqlx::query_as::<_, ShadowEventRow>(
            "SELECT id, session_id, turn_number, event_type, tool_id, risk_signal, \
             risk_level, probe_verdict, context_summary, created_at \
             FROM safety_shadow_events \
             WHERE tool_id = ? \
             ORDER BY created_at DESC \
             LIMIT ?",
        )
        .bind(tool_id)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AgentError::Db(e.to_string()))?;

        Ok(rows.into_iter().map(ShadowEvent::from).collect())
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

impl From<ShadowEventRow> for ShadowEvent {
    fn from(r: ShadowEventRow) -> Self {
        Self {
            id: r.id,
            session_id: r.session_id,
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
    session_id: String,
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
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            probe,
            config,
            probes_this_turn: AtomicU32::new(0),
            session_id: session_id.into(),
        }
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
        {
            return ToolRiskCategory::Shell;
        }
        if qualified_tool_id == "builtin:write"
            || qualified_tool_id == "builtin:edit"
            || qualified_tool_id == "builtin:delete"
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
                if pattern.contains("write") || pattern.contains("edit") || pattern.contains("file")
                {
                    if qualified_tool_id.starts_with("mcp:") {
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
        let trajectory = match self
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
        let event = ShadowEvent {
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
        tokio::spawn(async move {
            if let Err(e) = store.record(&event).await {
                tracing::warn!(error = %e, "ShadowSentinel: failed to persist probe result");
            }
        });

        verdict
    }

    /// Persist a tool execution event in the shadow stream (fire-and-forget).
    ///
    /// Called after a tool finishes execution to maintain the trajectory for future probes.
    pub fn record_tool_event(
        &self,
        qualified_tool_id: &str,
        turn_number: u64,
        risk_level: &str,
        context_summary: &str,
    ) {
        if !self.config.enabled {
            return;
        }
        let event = ShadowEvent {
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
        tokio::spawn(async move {
            if let Err(e) = store.record(&event).await {
                tracing::warn!(error = %e, "ShadowSentinel: failed to persist tool event");
            }
        });
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
                _: &'a [ShadowEvent],
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeVerdict> + Send + 'a>>
            {
                Box::pin(async { ProbeVerdict::Allow })
            }
        }
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite pool");
        let store = ShadowEventStore::new(pool);
        ShadowSentinel::new(store, Box::new(NoopProbe), config, "test-session")
    }
}
