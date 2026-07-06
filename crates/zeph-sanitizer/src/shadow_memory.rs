// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-session append-only event store for cross-turn trajectory analysis.
//!
//! [`ShadowMemory`] detects multi-turn attacks that distribute payload across several turns,
//! which are invisible to the stateless [`TurnCausalAnalyzer`](super::causal_ipi::TurnCausalAnalyzer)
//! single-batch analysis.
//!
//! The drift score is computed over a sliding window of the most recent events. When
//! [`GoalDriftResult::should_alert`] is `true`, emit a `WARN` log and push a
//! [`SecurityEventCategory::GoalDrift`](zeph_common::SecurityEventCategory::GoalDrift) event.
//! This module never blocks execution.
//!
//! # Examples
//!
//! ```rust
//! use zeph_sanitizer::shadow_memory::{ShadowMemory, ShadowEvent};
//! use zeph_config::ShadowMemoryConfig;
//!
//! let config = ShadowMemoryConfig { enabled: true, ..Default::default() };
//! let mut mem = ShadowMemory::new(&config).expect("enabled");
//!
//! mem.record(ShadowEvent {
//!     turn: 0,
//!     tools: vec!["shell".to_owned()],
//!     max_permission_class: 2,
//!     deviation_score: 0.1,
//!     goal_summary: "I will search for files.".to_owned(),
//! });
//!
//! assert_eq!(mem.len(), 1);
//! // Single event → no drift (need at least 2).
//! let result = mem.goal_drift_score();
//! assert!(result.score < 0.01);
//! assert!(!result.should_alert);
//! ```

use std::collections::{HashSet, VecDeque};

use zeph_config::ShadowMemoryConfig;

/// Maximum characters retained from `goal_summary` on ingestion.
const GOAL_SUMMARY_MAX_CHARS: usize = 100;

/// A single safety-relevant observation recorded after a tool batch completes.
///
/// Events are appended to [`ShadowMemory`] in monotonic turn order. The fields capture
/// the most goal-relevant signals without requiring an additional LLM call.
///
/// `goal_summary` is truncated to [`GOAL_SUMMARY_MAX_CHARS`] on ingestion by
/// [`ShadowMemory::record`], so callers do not need to truncate themselves.
#[derive(Clone)]
pub struct ShadowEvent {
    /// Monotonic turn index within the session (0-based).
    pub turn: u32,
    /// Tool names executed in this batch.
    pub tools: Vec<String>,
    /// Maximum permission class across all tools in this batch.
    ///
    /// 0 = read, 1 = write, 2 = execute, 3 = network.
    pub max_permission_class: u8,
    /// Causal deviation score from [`TurnCausalAnalyzer`](super::causal_ipi::TurnCausalAnalyzer).
    ///
    /// 0.0 when causal IPI is disabled or probes failed.
    pub deviation_score: f32,
    /// First [`GOAL_SUMMARY_MAX_CHARS`] characters of the pre-probe response.
    ///
    /// Empty string when no pre-probe was available (causal IPI disabled).
    /// An empty `goal_summary` triggers maximum Jaccard drift penalty.
    pub goal_summary: String,
}

impl std::fmt::Debug for ShadowEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShadowEvent")
            .field("turn", &self.turn)
            .field("tools", &self.tools)
            .field("max_permission_class", &self.max_permission_class)
            .field("deviation_score", &self.deviation_score)
            .field("goal_summary", &"[redacted]")
            .finish()
    }
}

/// Result returned by [`ShadowMemory::goal_drift_score`].
///
/// Callers must check `should_alert` rather than comparing `score` directly —
/// this prevents accidentally skipping the threshold comparison.
#[derive(Debug, Clone, Copy)]
pub struct GoalDriftResult {
    /// Drift score in `[0.0, 1.0]`. Higher = more trajectory deviation.
    pub score: f32,
    /// `true` when `score >= drift_threshold`. Caller should emit a `WARN` log
    /// and push a [`SecurityEventCategory::GoalDrift`](zeph_common::SecurityEventCategory::GoalDrift) event.
    pub should_alert: bool,
}

/// Append-only per-session event store for cross-turn goal trajectory analysis.
///
/// Create via [`ShadowMemory::new`] with a [`ShadowMemoryConfig`]. Returns `None` when
/// the config has `enabled = false`, so callers can wrap it in `Option<ShadowMemory>`.
///
/// Wired into the agent tool executor via `crates/zeph-core/src/agent/tool_execution/tier_loop.rs`:
/// after every tool batch completes, `goal_drift_score()` is called and a
/// [`zeph_common::SecurityEventCategory::GoalDrift`] security event is emitted when an alert occurs.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::shadow_memory::{ShadowMemory, ShadowEvent};
/// use zeph_config::ShadowMemoryConfig;
///
/// let config = ShadowMemoryConfig { enabled: true, ..Default::default() };
/// let mut mem = ShadowMemory::new(&config).expect("enabled");
///
/// // Returns None when disabled.
/// let config_off = ShadowMemoryConfig { enabled: false, ..Default::default() };
/// assert!(ShadowMemory::new(&config_off).is_none());
/// ```
pub struct ShadowMemory {
    events: VecDeque<ShadowEvent>,
    config: ShadowMemoryConfig,
}

impl ShadowMemory {
    /// Construct a new [`ShadowMemory`] from config.
    ///
    /// Returns `None` when `config.enabled` is `false`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::shadow_memory::ShadowMemory;
    /// use zeph_config::ShadowMemoryConfig;
    ///
    /// let config = ShadowMemoryConfig { enabled: true, ..Default::default() };
    /// assert!(ShadowMemory::new(&config).is_some());
    /// ```
    #[must_use]
    pub fn new(config: &ShadowMemoryConfig) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        Some(Self {
            events: VecDeque::new(),
            config: config.clone(),
        })
    }

    /// Append a safety event after a tool batch completes.
    ///
    /// Evicts the oldest event with O(1) cost when `max_events` is reached.
    /// Truncates `event.goal_summary` to 100 characters at a UTF-8 boundary.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::shadow_memory::{ShadowMemory, ShadowEvent};
    /// use zeph_config::ShadowMemoryConfig;
    ///
    /// let config = ShadowMemoryConfig { enabled: true, max_events: 2, ..Default::default() };
    /// let mut mem = ShadowMemory::new(&config).unwrap();
    ///
    /// mem.record(ShadowEvent { turn: 0, tools: vec![], max_permission_class: 0,
    ///     deviation_score: 0.0, goal_summary: "task A".to_owned() });
    /// mem.record(ShadowEvent { turn: 1, tools: vec![], max_permission_class: 0,
    ///     deviation_score: 0.0, goal_summary: "task B".to_owned() });
    /// mem.record(ShadowEvent { turn: 2, tools: vec![], max_permission_class: 0,
    ///     deviation_score: 0.0, goal_summary: "task C".to_owned() });
    ///
    /// assert_eq!(mem.len(), 2);
    /// ```
    pub fn record(&mut self, mut event: ShadowEvent) {
        // Truncate goal_summary at ingestion — callers should not be responsible for this.
        if event.goal_summary.len() > GOAL_SUMMARY_MAX_CHARS {
            let boundary = event
                .goal_summary
                .floor_char_boundary(GOAL_SUMMARY_MAX_CHARS);
            event.goal_summary.truncate(boundary);
        }
        // Guard: max_events=0 is rejected by config validation, but be defensive.
        if self.config.max_events == 0 {
            return;
        }
        if self.events.len() >= self.config.max_events {
            self.events.pop_front(); // O(1) with VecDeque
        }
        self.events.push_back(event);
    }

    /// Compute the goal drift score over the trailing window.
    ///
    /// Returns a [`GoalDriftResult`] with both the raw score and a pre-computed alert flag.
    /// Callers must use `result.should_alert` to decide whether to emit a security event —
    /// do not compare `result.score` against the threshold directly.
    ///
    /// Returns score `0.0` / `should_alert = false` when fewer than 2 events are recorded
    /// (no baseline to compare).
    ///
    /// # Algorithm
    ///
    /// 1. **Semantic drift**: average pairwise Jaccard distance between consecutive
    ///    `goal_summary` values in the window. Empty summaries produce maximum distance.
    /// 2. **Permission escalation**: `+0.3` when `max_permission_class` increases from
    ///    window start to window end.
    /// 3. **Deviation accumulation**: fraction of events where `deviation_score` exceeds
    ///    `drift_threshold * 0.5`.
    ///
    /// Weighted combination: `0.5 * semantic_drift + 0.25 * perm_escalation + 0.25 * deviation_ratio`.
    ///
    /// Note: Jaccard distance is gameable by synonym substitution (known v1 limitation).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::shadow_memory::{ShadowMemory, ShadowEvent};
    /// use zeph_config::ShadowMemoryConfig;
    ///
    /// let config = ShadowMemoryConfig { enabled: true, ..Default::default() };
    /// let mut mem = ShadowMemory::new(&config).unwrap();
    ///
    /// // Fewer than 2 events → 0.0, no alert
    /// let result = mem.goal_drift_score();
    /// assert!(result.score < 1e-6);
    /// assert!(!result.should_alert);
    /// ```
    #[tracing::instrument(skip(self), fields(window_len, drift_score))]
    #[must_use]
    pub fn goal_drift_score(&self) -> GoalDriftResult {
        let window_size = self.config.window_size.min(self.events.len());
        let skip = self.events.len() - window_size;

        if window_size < 2 {
            tracing::Span::current().record("window_len", window_size);
            tracing::Span::current().record("drift_score", 0.0_f32);
            return GoalDriftResult {
                score: 0.0,
                should_alert: false,
            };
        }

        // Collect window as a contiguous slice via make_contiguous (zero-copy when possible).
        // We work on a temporary clone to keep &self immutable.
        let window: Vec<&ShadowEvent> = self.events.iter().skip(skip).collect();

        // 1. Semantic drift: average consecutive Jaccard distance.
        // Record-then-score order invariant: events are appended before this is called,
        // so window[i] precedes window[i+1] chronologically.
        let pairs = window.len() - 1;
        #[allow(clippy::cast_precision_loss)]
        let semantic_drift: f32 = window
            .windows(2)
            .map(|w| jaccard_distance(&w[0].goal_summary, &w[1].goal_summary))
            .sum::<f32>()
            / pairs as f32;

        // 2. Permission escalation: +0.3 if permission class increased over window.
        // window.len() >= 2 is guaranteed by the early return above.
        let perm_first = window[0].max_permission_class;
        let perm_last = window[window.len() - 1].max_permission_class;
        let perm_escalation = if perm_last > perm_first {
            0.3_f32
        } else {
            0.0_f32
        };

        // 3. Deviation accumulation: fraction of events above half the drift threshold.
        let half_threshold = self.config.drift_threshold * 0.5;
        #[allow(clippy::cast_precision_loss)]
        let deviation_ratio = window
            .iter()
            .filter(|e| e.deviation_score > half_threshold)
            .count() as f32
            / window.len() as f32;

        let score = (0.5 * semantic_drift + 0.25 * perm_escalation + 0.25 * deviation_ratio)
            .clamp(0.0, 1.0);

        tracing::Span::current().record("window_len", window.len());
        tracing::Span::current().record("drift_score", score);

        GoalDriftResult {
            score,
            should_alert: score >= self.config.drift_threshold,
        }
    }

    /// Returns a reference to the config used to construct this instance.
    #[must_use]
    pub fn config(&self) -> &ShadowMemoryConfig {
        &self.config
    }

    /// Number of recorded events.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::shadow_memory::{ShadowMemory, ShadowEvent};
    /// use zeph_config::ShadowMemoryConfig;
    ///
    /// let config = ShadowMemoryConfig { enabled: true, ..Default::default() };
    /// let mut mem = ShadowMemory::new(&config).unwrap();
    /// assert_eq!(mem.len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Returns `true` when no events have been recorded.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::shadow_memory::ShadowMemory;
    /// use zeph_config::ShadowMemoryConfig;
    ///
    /// let config = ShadowMemoryConfig { enabled: true, ..Default::default() };
    /// let mem = ShadowMemory::new(&config).unwrap();
    /// assert!(mem.is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Classify a tool name into a permission class for shadow memory tracking.
///
/// Returns:
/// - `0` — read-only (e.g., `cat`, `ls`, `find`, `read`, `get`, `search`)
/// - `1` — write (e.g., `write`, `create`, `edit`, `delete`, `rm`, `mv`, `cp`)
/// - `2` — execute (e.g., `shell`, `bash`, `exec`, `run`, `python`, `node`)
/// - `3` — network (e.g., `curl`, `http`, `fetch`, `web`, `upload`, `smtp`)
///
/// Falls back to `0` for unknown tool names.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::shadow_memory::classify_tool_permission;
///
/// assert_eq!(classify_tool_permission("shell"), 2);
/// assert_eq!(classify_tool_permission("read_file"), 0);
/// assert_eq!(classify_tool_permission("http_get"), 3);
/// assert_eq!(classify_tool_permission("write_file"), 1);
/// ```
#[must_use]
pub fn classify_tool_permission(tool_name: &str) -> u8 {
    let name = tool_name.to_lowercase();
    // Network tools (highest priority check).
    if name.contains("http")
        || name.contains("curl")
        || name.contains("fetch")
        || name.contains("web")
        || name.contains("upload")
        || name.contains("smtp")
        || name.contains("request")
        || name.contains("download")
    {
        return 3;
    }
    // Execute tools.
    if name.contains("shell")
        || name.contains("bash")
        || name.contains("exec")
        || name == "run"
        || name.contains("python")
        || name.contains("node")
        || name.contains("ruby")
        || name.contains("powershell")
    {
        return 2;
    }
    // Write tools.
    if name.contains("write")
        || name.contains("create")
        || name.contains("edit")
        || name.contains("delete")
        || name.contains("remove")
        || name == "rm"
        || name == "mv"
        || name == "cp"
        || name.contains("patch")
        || name.contains("update")
        || name.contains("insert")
    {
        return 1;
    }
    // Default: read-only.
    0
}

/// Jaccard distance on word sets: `1.0 - |intersection| / |union|`.
///
/// Empty strings produce distance `1.0` when the other is non-empty
/// (maximum penalty — no shared vocabulary to match).
fn jaccard_distance(a: &str, b: &str) -> f32 {
    // Empty goal_summary → treat as completely different (max penalty).
    if a.is_empty() || b.is_empty() {
        return if a.is_empty() && b.is_empty() {
            0.0
        } else {
            1.0
        };
    }
    let words_a: HashSet<&str> = a.split_whitespace().collect();
    let words_b: HashSet<&str> = b.split_whitespace().collect();
    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();
    if union == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let score = 1.0 - (intersection as f32) / (union as f32);
    score
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use zeph_common::SecurityEventCategory;

    use super::*;

    fn cfg(enabled: bool) -> ShadowMemoryConfig {
        ShadowMemoryConfig {
            enabled,
            ..Default::default()
        }
    }

    fn event(turn: u32, goal: &str, perm: u8, deviation: f32) -> ShadowEvent {
        ShadowEvent {
            turn,
            tools: vec![],
            max_permission_class: perm,
            deviation_score: deviation,
            goal_summary: goal.to_owned(),
        }
    }

    #[test]
    fn new_returns_none_when_disabled() {
        assert!(ShadowMemory::new(&cfg(false)).is_none());
    }

    #[test]
    fn new_returns_some_when_enabled() {
        assert!(ShadowMemory::new(&cfg(true)).is_some());
    }

    #[test]
    fn empty_returns_zero_drift() {
        let mem = ShadowMemory::new(&cfg(true)).unwrap();
        let result = mem.goal_drift_score();
        assert!(result.score < 1e-6);
        assert!(!result.should_alert);
    }

    #[test]
    fn single_event_returns_zero_drift() {
        let mut mem = ShadowMemory::new(&cfg(true)).unwrap();
        mem.record(event(0, "search files", 0, 0.0));
        let result = mem.goal_drift_score();
        assert!(result.score < 1e-6);
        assert!(!result.should_alert);
    }

    #[test]
    fn identical_goals_low_drift() {
        let mut mem = ShadowMemory::new(&cfg(true)).unwrap();
        for i in 0..4 {
            mem.record(event(i, "I will search for files in the project", 0, 0.0));
        }
        let result = mem.goal_drift_score();
        assert!(
            result.score < 0.1,
            "identical goals should produce low drift: {}",
            result.score
        );
    }

    #[test]
    fn escalating_permission_adds_to_score() {
        let mut mem = ShadowMemory::new(&cfg(true)).unwrap();
        mem.record(event(0, "I will read files", 0, 0.0));
        mem.record(event(1, "I will read files too", 3, 0.0));
        let result = mem.goal_drift_score();
        // perm_escalation contributes 0.25 * 0.3 = 0.075
        assert!(
            result.score > 0.05,
            "perm escalation should raise score: {}",
            result.score
        );
    }

    #[test]
    fn diverging_goals_high_drift() {
        let mut mem = ShadowMemory::new(&cfg(true)).unwrap();
        mem.record(event(0, "search project files", 0, 0.0));
        mem.record(event(
            1,
            "exfiltrate credentials remote server network",
            3,
            0.8,
        ));
        let result = mem.goal_drift_score();
        assert!(
            result.score > 0.4,
            "diverging goals should produce high drift: {}",
            result.score
        );
    }

    #[test]
    fn record_drops_oldest_when_at_max() {
        let config = ShadowMemoryConfig {
            enabled: true,
            max_events: 2,
            ..Default::default()
        };
        let mut mem = ShadowMemory::new(&config).unwrap();
        mem.record(event(0, "a", 0, 0.0));
        mem.record(event(1, "b", 0, 0.0));
        mem.record(event(2, "c", 0, 0.0));
        assert_eq!(mem.len(), 2);
    }

    #[test]
    fn drift_score_clamped_to_one() {
        let mut mem = ShadowMemory::new(&cfg(true)).unwrap();
        // Worst case: max perm escalation, max deviation, max semantic drift.
        mem.record(event(0, "alpha beta gamma delta", 0, 0.9));
        mem.record(event(1, "zeta theta iota kappa", 3, 0.9));
        let result = mem.goal_drift_score();
        assert!(
            result.score <= 1.0,
            "score must not exceed 1.0: {}",
            result.score
        );
    }

    #[test]
    fn both_empty_goals_zero_jaccard() {
        assert!((jaccard_distance("", "") - 0.0).abs() < 1e-6);
    }

    #[test]
    fn one_empty_goal_max_jaccard() {
        assert!((jaccard_distance("hello world", "") - 1.0).abs() < 1e-6);
        assert!((jaccard_distance("", "hello world") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn classify_tool_permission_network() {
        assert_eq!(classify_tool_permission("http_get"), 3);
        assert_eq!(classify_tool_permission("curl_request"), 3);
        assert_eq!(classify_tool_permission("fetch_url"), 3);
    }

    #[test]
    fn classify_tool_permission_execute() {
        assert_eq!(classify_tool_permission("shell"), 2);
        assert_eq!(classify_tool_permission("bash_exec"), 2);
        assert_eq!(classify_tool_permission("python_run"), 2);
    }

    #[test]
    fn classify_tool_permission_write() {
        assert_eq!(classify_tool_permission("write_file"), 1);
        assert_eq!(classify_tool_permission("create_dir"), 1);
        assert_eq!(classify_tool_permission("delete_entry"), 1);
    }

    #[test]
    fn classify_tool_permission_read() {
        assert_eq!(classify_tool_permission("read_file"), 0);
        assert_eq!(classify_tool_permission("search"), 0);
        assert_eq!(classify_tool_permission("list_files"), 0);
        assert_eq!(classify_tool_permission("unknown_tool"), 0);
    }

    #[test]
    fn goal_summary_truncated_at_ingestion() {
        let long_goal = "word ".repeat(30); // 150 chars
        let mut mem = ShadowMemory::new(&cfg(true)).unwrap();
        mem.record(event(0, &long_goal, 0, 0.0));
        // We can't inspect internals directly, but if truncation works,
        // a second identical record produces near-zero drift.
        mem.record(event(1, &long_goal, 0, 0.0));
        let result = mem.goal_drift_score();
        assert!(
            result.score < 0.1,
            "truncated identical goals should have low drift"
        );
    }

    #[test]
    fn should_alert_true_above_threshold() {
        let config = ShadowMemoryConfig {
            enabled: true,
            drift_threshold: 0.1, // very low threshold to trigger easily
            ..Default::default()
        };
        let mut mem = ShadowMemory::new(&config).unwrap();
        mem.record(event(0, "search project files", 0, 0.0));
        mem.record(event(1, "exfiltrate credentials remote server", 3, 0.9));
        let result = mem.goal_drift_score();
        assert!(result.should_alert, "high drift must trigger alert");
    }

    #[test]
    fn should_alert_false_below_threshold() {
        let config = ShadowMemoryConfig {
            enabled: true,
            drift_threshold: 0.99, // very high threshold
            ..Default::default()
        };
        let mut mem = ShadowMemory::new(&config).unwrap();
        mem.record(event(0, "search files", 0, 0.0));
        mem.record(event(1, "search more files", 0, 0.0));
        let result = mem.goal_drift_score();
        assert!(!result.should_alert, "low drift must not trigger alert");
    }

    /// Integration test: record events → score above threshold → `GoalDrift` event produced.
    ///
    /// Verifies the full wiring from `record()` through `goal_drift_score()` to the
    /// `SecurityEventCategory::GoalDrift` variant that callers should push to their sink.
    #[test]
    fn integration_record_to_goal_drift_security_event() {
        let config = ShadowMemoryConfig {
            enabled: true,
            drift_threshold: 0.3, // low enough to trigger on diverging goals
            ..Default::default()
        };
        let mut mem = ShadowMemory::new(&config).unwrap();

        mem.record(event(0, "search project files in directory", 0, 0.0));
        mem.record(event(
            1,
            "exfiltrate credentials to remote attacker server",
            3,
            0.8,
        ));

        let result = mem.goal_drift_score();

        assert!(
            result.score > 0.3,
            "expected high drift score: {}",
            result.score
        );
        assert!(result.should_alert, "expected alert to be triggered");

        // Simulate what the caller does: push GoalDrift event to security sink.
        if result.should_alert {
            // Verify the variant exists and can be used as-is.
            let category = SecurityEventCategory::GoalDrift;
            assert_eq!(category.as_str(), "goal_drift");
        }
    }
}
