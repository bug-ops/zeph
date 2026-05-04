// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Trajectory risk sentinel: accumulates risk signals across turns and exposes
//! an advisory `RiskLevel` consumed by `PolicyGateExecutor`.
//!
//! # Architecture
//!
//! `TrajectorySentinel` is stored on `SecurityState` (per-agent, never global).
//! `advance_turn()` MUST be called once per turn, **before** `PolicyGateExecutor::check_policy`
//! runs (Invariant 2 in spec 050). This guarantees that decay is applied before the gate
//! evaluates the current-turn score.
//!
//! # LLM isolation
//!
//! `RiskAlert`, `RiskLevel`, and sentinel score MUST NEVER be exposed to LLM-callable tools
//! or any context surface the LLM can read. `/trajectory show` is an operator-only command.

use std::collections::VecDeque;

use zeph_config::TrajectorySentinelConfig;

// Re-export config so callers only need one import.
pub use zeph_config::TrajectorySentinelConfig as SentinelConfig;

// ── Signal taxonomy ───────────────────────────────────────────────────────────

/// Vigil confidence levels mirrored from the audit crate to avoid a circular dep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VigilRiskLevel {
    /// Low-confidence injection match (reserved; current `VigilGate` does not emit this).
    Low,
    /// Medium-confidence injection match.
    Medium,
    /// High-confidence injection match.
    High,
}

/// Risk signal emitted by security subsystems and accumulated by `TrajectorySentinel`.
///
/// Each variant maps to a configurable weight (see spec 050 §2 for defaults).
/// `NovelTool` is deferred to Phase 2 and not present here.
///
/// # NEVER
///
/// Never expose signal values or the accumulated score to any LLM-callable surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskSignal {
    /// VIGIL flagged a tool output with the given confidence level.
    VigilFlagged(VigilRiskLevel),
    /// `PolicyEnforcer` denied a structured tool call.
    PolicyDeny,
    /// `ExfiltrationGuard` redacted at least one outbound URL or HTML img.
    ExfiltrationRedaction,
    /// Tool call rejected as out-of-scope by `ScopedToolExecutor`.
    OutOfScope,
    /// PII filter redacted ≥ 1 span in a tool output.
    PiiRedaction,
    /// Tool returned a non-zero exit code or unrecoverable error.
    ToolFailure,
    /// More than `high_call_rate_threshold` tool calls in the last 3 turns.
    HighCallRate,
    /// More than `unusual_read_threshold` distinct paths read in `window_turns`.
    UnusualReadVolume,
    /// A configured high-risk tool-pair transition occurred within K turns.
    ToolPairTransition,
}

impl RiskSignal {
    /// Returns the default weight for this signal (configurable in Phase 2).
    ///
    /// Weights are finite and non-negative; this upholds the NEVER-negative-score invariant.
    #[must_use]
    pub fn default_weight(self) -> f32 {
        match self {
            Self::VigilFlagged(VigilRiskLevel::High) => 2.5,
            Self::VigilFlagged(VigilRiskLevel::Medium) => 1.0,
            Self::ExfiltrationRedaction | Self::ToolPairTransition => 2.0,
            Self::PolicyDeny | Self::OutOfScope | Self::HighCallRate | Self::UnusualReadVolume => {
                1.5
            }
            Self::PiiRedaction => 0.5,
            // VigilFlagged(Low) and ToolFailure are both noisy low-weight signals.
            Self::VigilFlagged(VigilRiskLevel::Low) | Self::ToolFailure => 0.3,
        }
    }
}

impl RiskSignal {
    /// Convert a `u8` signal code from `RiskSignalSink` callbacks into a `RiskSignal`.
    ///
    /// Code table (mirrors the numeric constants used in `zeph-tools`):
    /// - `1` = `PolicyDeny`
    /// - `2` = `ExfiltrationRedaction`
    /// - `3` = `OutOfScope`
    /// - `4` = `PiiRedaction`
    /// - `5` = `ToolFailure`
    /// - `6` = `VigilFlagged(Medium)`
    /// - `7` = `VigilFlagged(High)`
    /// - anything else = `VigilFlagged(Low)` (fallback)
    #[must_use]
    pub fn from_code(code: u8) -> Self {
        match code {
            1 => Self::PolicyDeny,
            2 => Self::ExfiltrationRedaction,
            3 => Self::OutOfScope,
            4 => Self::PiiRedaction,
            5 => Self::ToolFailure,
            6 => Self::VigilFlagged(VigilRiskLevel::Medium),
            7 => Self::VigilFlagged(VigilRiskLevel::High),
            _ => Self::VigilFlagged(VigilRiskLevel::Low),
        }
    }
}

// ── Risk levels ───────────────────────────────────────────────────────────────

/// Advisory risk level computed from the accumulated score.
///
/// `PolicyGateExecutor` consumes this to decide whether to downgrade an `Allow` decision.
///
/// # LLM isolation
///
/// This enum MUST NOT appear in any tool output, slash-command response, or LLM context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    /// Score < `elevated_at`. Normal operation.
    Calm,
    /// Score in `[elevated_at, high_at)`. Audit tag only.
    Elevated,
    /// Score in `[high_at, critical_at)`. Audit tag + `RiskAlert` emitted.
    High,
    /// Score >= `critical_at`. `Allow` decisions downgraded to `Deny`.
    Critical,
}

impl From<RiskLevel> for u8 {
    fn from(level: RiskLevel) -> Self {
        match level {
            RiskLevel::Calm => 0,
            RiskLevel::Elevated => 1,
            RiskLevel::High => 2,
            RiskLevel::Critical => 3,
        }
    }
}

// ── Risk alert ────────────────────────────────────────────────────────────────

/// Emitted when the score crosses `alert_threshold`.
///
/// Consumed by `PolicyGateExecutor`. MUST NOT be observable by LLM-callable tools.
#[derive(Debug, Clone, Copy)]
pub struct RiskAlert {
    /// Current risk level at alert time.
    pub level: RiskLevel,
    /// Accumulated score at alert time (rounded to two decimal places for logs).
    pub score: f32,
}

// ── Sentinel ──────────────────────────────────────────────────────────────────

/// Cross-turn risk accumulator for the advisory trajectory governance layer.
///
/// # Usage
///
/// ```rust
/// use zeph_core::agent::trajectory::{TrajectorySentinel, RiskSignal, RiskLevel, VigilRiskLevel};
/// use zeph_config::TrajectorySentinelConfig;
///
/// let mut sentinel = TrajectorySentinel::new(TrajectorySentinelConfig::default());
///
/// // Call advance_turn once per turn, BEFORE gate evaluation.
/// let _ = sentinel.advance_turn();
/// sentinel.record(RiskSignal::VigilFlagged(VigilRiskLevel::High));
/// sentinel.record(RiskSignal::PolicyDeny);
///
/// let level = sentinel.current_risk();
/// assert!(level >= RiskLevel::Calm);
/// ```
pub struct TrajectorySentinel {
    cfg: TrajectorySentinelConfig,
    /// Ring buffer of `(turn_number, signal)` pairs; evicted outside `window_turns`.
    buf: VecDeque<(u64, RiskSignal)>,
    current_turn: u64,
    /// Turn on which the score last changed (for `advance_turn` dirty-tracking).
    last_signal_turn: u64,
    /// Cached sum; `None` means the buffer was mutated since the last computation.
    cached_score: Option<f32>,
    /// How many consecutive turns the sentinel has been at `>= Critical`.
    critical_consecutive_turns: u32,
}

impl TrajectorySentinel {
    /// Create a fresh sentinel with the given configuration.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_core::agent::trajectory::TrajectorySentinel;
    /// use zeph_config::TrajectorySentinelConfig;
    ///
    /// let sentinel = TrajectorySentinel::new(TrajectorySentinelConfig::default());
    /// ```
    #[must_use]
    pub fn new(cfg: TrajectorySentinelConfig) -> Self {
        Self {
            cfg,
            buf: VecDeque::new(),
            current_turn: 0,
            last_signal_turn: 0,
            cached_score: Some(0.0),
            critical_consecutive_turns: 0,
        }
    }

    /// Initialise a child sentinel for a spawned subagent per FR-CG-011.
    ///
    /// When the parent is at `>= Elevated`, the child starts with a damped copy of the
    /// parent's score (`parent_score * subagent_inheritance_factor`). This prevents
    /// a subagent spawn from acting as a free risk reset.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_core::agent::trajectory::{TrajectorySentinel, RiskSignal, RiskLevel, VigilRiskLevel};
    /// use zeph_config::TrajectorySentinelConfig;
    ///
    /// let mut parent = TrajectorySentinel::new(TrajectorySentinelConfig::default());
    /// let _ = parent.advance_turn();
    /// parent.record(RiskSignal::VigilFlagged(VigilRiskLevel::High));
    /// parent.record(RiskSignal::PolicyDeny);
    ///
    /// let child = parent.spawn_child();
    /// // Child starts with some inherited score when parent is >= Elevated.
    /// ```
    #[must_use]
    pub fn spawn_child(&self) -> TrajectorySentinel {
        let mut child = TrajectorySentinel::new(self.cfg.clone());
        if self.current_risk() >= RiskLevel::Elevated {
            let parent_score = self.score_now();
            let damped = parent_score * self.cfg.subagent_inheritance_factor;
            child.seed_score(damped);
        }
        child
    }

    /// Advance the turn counter and apply multiplicative decay.
    ///
    /// MUST be called once per turn, **before** any `PolicyGateExecutor::check_policy` runs.
    /// Also handles the FR-CG-010 auto-recover cap: after `auto_recover_after_turns`
    /// consecutive turns at `Critical` with no new high-weight signal, the score is hard-reset
    /// to `0.0` and the buffer is cleared.
    ///
    /// Returns `true` when auto-recover fired this turn — the caller MUST write an audit entry
    /// with `error_category = "trajectory_auto_recover"` (F5 requirement).
    #[must_use]
    pub fn advance_turn(&mut self) -> bool {
        self.current_turn += 1;
        self.cached_score = None; // score must be recomputed after decay

        // Evict signals outside the window.
        let window = u64::from(self.cfg.window_turns);
        while let Some(&(turn, _)) = self.buf.front() {
            if self.current_turn.saturating_sub(turn) >= window {
                self.buf.pop_front();
            } else {
                break;
            }
        }

        // Track Critical consecutive turns for auto-recover (FR-CG-010).
        if self.current_risk() >= RiskLevel::Critical {
            self.critical_consecutive_turns += 1;
            let cap = self.cfg.auto_recover_after_turns.max(4); // floor at 4
            if self.critical_consecutive_turns >= cap {
                let score_at_reset = self.score_now();
                let signal_census = self.buf.len();
                tracing::warn!(
                    score = score_at_reset,
                    signal_count = signal_census,
                    turns_at_critical = self.critical_consecutive_turns,
                    "trajectory auto-recover: hard reset after {} consecutive Critical turns",
                    cap
                );
                self.buf.clear();
                self.cached_score = Some(0.0);
                self.critical_consecutive_turns = 0;
                return true;
            }
        } else {
            self.critical_consecutive_turns = 0;
        }
        false
    }

    /// Record a risk signal for the current turn.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_core::agent::trajectory::{TrajectorySentinel, RiskSignal};
    /// use zeph_config::TrajectorySentinelConfig;
    ///
    /// let mut sentinel = TrajectorySentinel::new(TrajectorySentinelConfig::default());
    /// let _ = sentinel.advance_turn();
    /// sentinel.record(RiskSignal::PolicyDeny);
    /// assert!(sentinel.score_now() > 0.0);
    /// ```
    pub fn record(&mut self, sig: RiskSignal) {
        self.buf.push_back((self.current_turn, sig));
        self.cached_score = None;
        self.last_signal_turn = self.current_turn;
    }

    /// Return the current risk level bucket for the accumulated score.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_core::agent::trajectory::{TrajectorySentinel, RiskLevel};
    /// use zeph_config::TrajectorySentinelConfig;
    ///
    /// let sentinel = TrajectorySentinel::new(TrajectorySentinelConfig::default());
    /// assert_eq!(sentinel.current_risk(), RiskLevel::Calm);
    /// ```
    #[must_use]
    pub fn current_risk(&self) -> RiskLevel {
        let score = self.score_now();
        if score >= self.cfg.critical_at {
            RiskLevel::Critical
        } else if score >= self.cfg.high_at {
            RiskLevel::High
        } else if score >= self.cfg.elevated_at {
            RiskLevel::Elevated
        } else {
            RiskLevel::Calm
        }
    }

    /// Return a `RiskAlert` when the score crosses `alert_threshold`, `None` otherwise.
    ///
    /// Consumed by `PolicyGateExecutor`. Never expose to LLM-callable surfaces.
    #[must_use]
    pub fn poll_alert(&self) -> Option<RiskAlert> {
        let score = self.score_now();
        if score >= self.cfg.alert_threshold {
            Some(RiskAlert {
                level: self.current_risk(),
                score,
            })
        } else {
            None
        }
    }

    /// Compute the decayed score from the signal buffer without mutating state.
    ///
    /// Score formula: `Σ_k decay_per_turn^(current_turn - signal_turn_k) * weight(signal_k)`
    ///
    /// Guaranteed to be finite and non-negative (upholds NEVER-negative invariant).
    #[must_use]
    pub fn score_now(&self) -> f32 {
        if let Some(cached) = self.cached_score {
            return cached;
        }
        let mut score: f32 = 0.0;
        let decay = self.cfg.decay_per_turn;
        for &(turn, signal) in &self.buf {
            #[allow(clippy::cast_precision_loss)]
            let age =
                u32::try_from(self.current_turn.saturating_sub(turn)).unwrap_or(u32::MAX) as f32;
            let contribution = decay.powf(age) * signal.default_weight();
            score += contribution;
        }
        // Clamp to non-negative to satisfy the invariant (floating-point rounding safety).
        score.max(0.0)
    }

    /// Hard reset: clear all state. Called on `/clear`, `/trajectory reset`, or session restart.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.cached_score = Some(0.0);
        self.critical_consecutive_turns = 0;
        self.last_signal_turn = 0;
    }

    /// Seed the sentinel with an initial score for subagent inheritance.
    ///
    /// Inserts a synthetic signal at turn 0 with the given weight. Only called
    /// from `spawn_child` — not part of the normal signal path.
    fn seed_score(&mut self, score: f32) {
        debug_assert!(score >= 0.0, "seed score must be non-negative");
        // Store a sentinel marker in the buffer so the seed participates in decay on the
        // next advance_turn(). We encode it as (turn=0, PolicyDeny) × N where N is
        // the number of PolicyDeny weights that sum to score. This is approximate but
        // correct in terms of decay behavior.
        let weight = RiskSignal::PolicyDeny.default_weight();
        // Use floor to avoid overshooting the parent's score (P2 requirement).
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let reps = (score / weight).floor() as usize;
        for _ in 0..reps {
            self.buf.push_back((0, RiskSignal::PolicyDeny));
        }
        self.cached_score = None; // will be recomputed from buf
    }

    /// The current turn counter (for diagnostics and audit logging only).
    #[must_use]
    pub fn current_turn(&self) -> u64 {
        self.current_turn
    }

    /// Number of signals in the current window (for diagnostics only).
    #[must_use]
    pub fn signal_count(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::TrajectorySentinelConfig;

    fn default_sentinel() -> TrajectorySentinel {
        TrajectorySentinel::new(TrajectorySentinelConfig::default())
    }

    #[test]
    fn fresh_sentinel_is_calm() {
        let s = default_sentinel();
        assert_eq!(s.current_risk(), RiskLevel::Calm);
        assert!(s.score_now().abs() < f32::EPSILON);
    }

    #[test]
    fn single_policy_deny_elevates_score() {
        let mut s = default_sentinel();
        let _ = s.advance_turn();
        s.record(RiskSignal::PolicyDeny);
        // PolicyDeny weight = 1.5, elevated_at = 2.0 → still Calm
        assert_eq!(s.current_risk(), RiskLevel::Calm);
        assert!((s.score_now() - 1.5).abs() < 0.01);
    }

    #[test]
    fn two_policy_denies_cross_elevated() {
        let mut s = default_sentinel();
        let _ = s.advance_turn();
        s.record(RiskSignal::PolicyDeny);
        s.record(RiskSignal::PolicyDeny);
        // 1.5 + 1.5 = 3.0 >= elevated_at(2.0)
        assert_eq!(s.current_risk(), RiskLevel::Elevated);
    }

    #[test]
    fn vigil_high_signals_drive_to_critical() {
        let mut s = default_sentinel();
        // 6 × VigilFlagged(High) over 8 turns → acceptance test from spec
        for _ in 0..6 {
            let _ = s.advance_turn();
            s.record(RiskSignal::VigilFlagged(VigilRiskLevel::High));
        }
        // Σ_{k=0..5} 0.85^k × 2.5 ≈ 10.3 >= critical_at(8.0)
        let score = s.score_now();
        assert!(score >= 8.0, "expected score >= 8.0, got {score}");
        assert_eq!(s.current_risk(), RiskLevel::Critical);
    }

    #[test]
    fn advance_turn_before_gate_ordering() {
        // Invariant 2: decay is applied at advance_turn, not at check time.
        let mut s = default_sentinel();
        let _ = s.advance_turn();
        s.record(RiskSignal::VigilFlagged(VigilRiskLevel::High)); // weight 2.5
        let score_turn1 = s.score_now();
        let _ = s.advance_turn();
        let score_turn2 = s.score_now();
        // After one idle turn, score decays by 0.85.
        assert!(
            score_turn2 < score_turn1,
            "score must decay after advance_turn"
        );
        assert!((score_turn2 - score_turn1 * 0.85).abs() < 0.01);
    }

    #[test]
    fn reset_clears_all_state() {
        let mut s = default_sentinel();
        let _ = s.advance_turn();
        s.record(RiskSignal::PolicyDeny);
        s.record(RiskSignal::PolicyDeny);
        assert!(s.current_risk() >= RiskLevel::Elevated);
        s.reset();
        assert_eq!(s.current_risk(), RiskLevel::Calm);
        assert!(s.score_now().abs() < f32::EPSILON);
    }

    #[test]
    fn auto_recover_after_critical_turns_hard_reset() {
        // decay_per_turn = 1.0 (no decay) and large window prevent score decay from
        // masking the hard-reset code path.  Cap is 4 turns to keep the test fast.
        let cfg = TrajectorySentinelConfig {
            auto_recover_after_turns: 4,
            window_turns: 30,
            decay_per_turn: 1.0,
            ..Default::default()
        };
        let mut s = TrajectorySentinel::new(cfg);

        // Prime to Critical: 4 × VigilFlagged(High) (weight 2.5 × 4 = 10.0 > critical_at 8.0).
        // With decay=1.0, score does not decay between turns.
        for _ in 0..4 {
            let _ = s.advance_turn();
            s.record(RiskSignal::VigilFlagged(VigilRiskLevel::High));
        }
        assert_eq!(
            s.current_risk(),
            RiskLevel::Critical,
            "must be Critical before sustain loop"
        );

        // Each advance_turn in this loop sees Critical (score=10.0, no decay).
        // critical_consecutive_turns increments each turn; hard-reset fires at turn 4.
        let mut recovered = false;
        for i in 0..4 {
            let fired = s.advance_turn();
            if fired {
                recovered = true;
                assert_eq!(
                    i, 3,
                    "hard-reset must fire on the 4th consecutive Critical turn, not turn {i}"
                );
                break;
            }
            assert_eq!(
                s.current_risk(),
                RiskLevel::Critical,
                "must stay Critical during sustain loop (turn {i})"
            );
        }
        assert!(
            recovered,
            "auto-recover hard-reset must fire after 4 consecutive Critical turns"
        );
        assert!(
            s.current_risk() < RiskLevel::Critical,
            "sentinel must be below Critical after hard-reset"
        );
        assert!(
            s.score_now().abs() < f32::EPSILON,
            "score must be 0 after hard-reset"
        );
    }

    #[test]
    fn score_never_negative() {
        // Property: random Phase-1 signal traces must never produce negative score.
        let mut s = default_sentinel();
        for _ in 0..20 {
            let _ = s.advance_turn();
            s.record(RiskSignal::ToolFailure);
            s.record(RiskSignal::PiiRedaction);
            assert!(s.score_now() >= 0.0, "score became negative");
        }
    }

    #[test]
    fn score_never_nan() {
        let mut s = default_sentinel();
        for _ in 0..20 {
            let _ = s.advance_turn();
            s.record(RiskSignal::VigilFlagged(VigilRiskLevel::High));
            assert!(!s.score_now().is_nan(), "score became NaN");
        }
    }

    #[test]
    fn spawn_child_inherits_score_when_elevated() {
        let mut parent = TrajectorySentinel::new(TrajectorySentinelConfig::default());
        let _ = parent.advance_turn();
        parent.record(RiskSignal::PolicyDeny);
        parent.record(RiskSignal::PolicyDeny);
        // parent at Elevated (score ~3.0)
        assert!(parent.current_risk() >= RiskLevel::Elevated);
        let child = parent.spawn_child();
        assert!(
            child.score_now() > 0.0,
            "child must inherit non-zero score from elevated parent"
        );
        assert!(
            child.score_now() < parent.score_now(),
            "child score must be damped relative to parent"
        );
    }

    #[test]
    fn spawn_child_no_inheritance_when_calm() {
        let parent = TrajectorySentinel::new(TrajectorySentinelConfig::default());
        assert_eq!(parent.current_risk(), RiskLevel::Calm);
        let child = parent.spawn_child();
        assert!(
            child.score_now().abs() < f32::EPSILON,
            "calm parent must not seed child"
        );
    }

    #[test]
    fn poll_alert_fires_at_alert_threshold() {
        let mut s = default_sentinel();
        let _ = s.advance_turn();
        // alert_threshold = 4.0; two VigilFlagged(High) at same turn = 5.0 >= 4.0
        s.record(RiskSignal::VigilFlagged(VigilRiskLevel::High));
        s.record(RiskSignal::VigilFlagged(VigilRiskLevel::High));
        let alert = s.poll_alert();
        assert!(alert.is_some(), "alert must fire at >= alert_threshold");
    }

    #[test]
    fn window_evicts_old_signals() {
        let cfg = TrajectorySentinelConfig {
            window_turns: 3,
            ..Default::default()
        };
        let mut s = TrajectorySentinel::new(cfg);
        let _ = s.advance_turn();
        s.record(RiskSignal::VigilFlagged(VigilRiskLevel::High)); // turn 1
        // Advance 3 more turns — the signal should be evicted.
        let _ = s.advance_turn(); // turn 2
        let _ = s.advance_turn(); // turn 3
        let _ = s.advance_turn(); // turn 4 — turn 1 signal is now >= window_turns old
        assert_eq!(
            s.signal_count(),
            0,
            "signals outside window must be evicted"
        );
    }

    #[test]
    fn trajectory_config_validation_decay_bounds() {
        let cfg_zero = TrajectorySentinelConfig {
            decay_per_turn: 0.0,
            ..Default::default()
        };
        assert!(
            cfg_zero.validate().is_err(),
            "decay=0.0 must fail validation"
        );
        let cfg_over = TrajectorySentinelConfig {
            decay_per_turn: 1.1,
            ..Default::default()
        };
        assert!(
            cfg_over.validate().is_err(),
            "decay>1.0 must fail validation"
        );
        let cfg_ok = TrajectorySentinelConfig {
            decay_per_turn: 0.85,
            ..Default::default()
        };
        assert!(cfg_ok.validate().is_ok());
    }

    #[test]
    fn trajectory_config_validation_threshold_ordering() {
        let cfg = TrajectorySentinelConfig {
            elevated_at: 5.0,
            high_at: 3.0, // violates elevated_at < high_at
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }
}
