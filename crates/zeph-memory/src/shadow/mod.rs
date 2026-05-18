// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MAGE shadow memory stream — trajectory-level risk accumulation (spec 004-16).
//!
//! [`TrajectoryRiskAccumulator`] maintains a per-session rolling risk score by ingesting
//! [`AuditSignalType`] events from `zeph-sanitizer`. The score decays exponentially between
//! turns and is used to gate tool execution when it exceeds a configured threshold.
//!
//! When `enabled = false` (default), every method is a zero-cost no-op — no allocations,
//! no computation.
//!
//! # Example
//!
//! ```rust
//! use zeph_memory::shadow::{TrajectoryRiskAccumulator, AuditSignalType, Severity};
//! use zeph_config::TrajectoryRiskAccumulatorConfig;
//!
//! let mut acc = TrajectoryRiskAccumulator::new_noop();
//! assert_eq!(acc.current_risk(), 0.0);
//! assert!(!acc.is_blocked());
//! ```

use std::collections::VecDeque;

use tracing::info_span;
use zeph_config::TrajectoryRiskAccumulatorConfig;

/// Signal type for a safety event ingested by [`TrajectoryRiskAccumulator`].
///
/// Maps to the four signal classes defined in spec 004-16, FR-007.
/// Callers in `zeph-core` convert from `zeph_sanitizer::audit::AuditSignalType` to this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuditSignalType {
    /// A policy gate denied or flagged an operation.
    PolicyViolation,
    /// A prompt-injection pattern was detected in untrusted content.
    PromptInjectionPattern,
    /// An anomalous tool-call chain was observed.
    ToolChainAnomaly,
    /// LLM response confidence dropped significantly between turns.
    ConfidenceDrop,
}

/// Severity level for an [`AuditSignalType`] ingested by [`TrajectoryRiskAccumulator`].
///
/// Mapped to a numeric multiplier by `TrajectorySeverityMultipliers`:
/// `Low → 0.5`, `Medium → 1.0`, `High → 2.0` (defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    /// Minor or likely-benign signal.
    Low,
    /// Moderate concern; warrants accumulation.
    Medium,
    /// Strong indicator; highest multiplier.
    High,
}

/// A recorded safety signal ingested during a specific turn.
#[derive(Debug, Clone)]
pub struct SignalEvent {
    /// Turn index at which the signal was ingested.
    pub turn_id: u32,
    /// Category of the detected signal.
    pub signal_type: AuditSignalType,
    /// Severity of the detected signal.
    pub severity: Severity,
    /// Computed contribution: `base_weight × severity_multiplier`.
    pub raw_score: f64,
}

/// Per-session trajectory risk accumulator (MAGE spec 004-16).
///
/// Maintains a rolling `trajectory_risk` score in `[0.0, 1.0]` that accumulates safety
/// signals with exponential temporal decay. Designed to detect multi-turn attacks that
/// evade per-turn controls.
///
/// When constructed via [`new_noop`][`TrajectoryRiskAccumulator::new_noop`] or when
/// `config.enabled = false`, **all methods are zero-cost no-ops** — no allocations and
/// `current_risk()` always returns `0.0`.
pub struct TrajectoryRiskAccumulator {
    /// `None` means noop mode — all operations are skipped.
    config: Option<TrajectoryRiskAccumulatorConfig>,
    /// Current accumulated risk score, clamped to `[0.0, 1.0]`.
    trajectory_risk: f64,
    /// Number of `advance_turn` calls since creation.
    turn_count: u32,
    /// Capped ring buffer of the most recent ingested signals.
    signal_history: VecDeque<SignalEvent>,
}

impl TrajectoryRiskAccumulator {
    /// Construct an accumulator that operates as a zero-cost noop.
    ///
    /// Use when shadow memory is disabled or during testing scenarios that do not need
    /// risk tracking. No heap allocation is performed.
    #[must_use]
    pub fn new_noop() -> Self {
        Self {
            config: None,
            trajectory_risk: 0.0,
            turn_count: 0,
            signal_history: VecDeque::new(),
        }
    }

    /// Construct an accumulator from configuration.
    ///
    /// When `config.enabled = false`, delegates to [`new_noop`][Self::new_noop] — no
    /// allocation. When enabled, pre-allocates the signal history ring buffer.
    #[must_use]
    pub fn new(config: TrajectoryRiskAccumulatorConfig) -> Self {
        if !config.enabled {
            return Self::new_noop();
        }
        let cap = config.signal_history_cap;
        Self {
            config: Some(config),
            trajectory_risk: 0.0,
            turn_count: 0,
            signal_history: VecDeque::with_capacity(cap.min(1024)),
        }
    }

    /// Advance the turn counter and apply exponential decay to the accumulated risk.
    ///
    /// Must be called **once per turn, before** [`ingest`][Self::ingest] is called for
    /// that turn. Decay formula: `risk *= exp(-ln(2) / halflife_turns)`.
    ///
    /// No-op when disabled.
    pub fn advance_turn(&mut self) {
        let _span = info_span!("memory.shadow.advance_turn").entered();
        let Some(config) = &self.config else { return };
        self.turn_count = self.turn_count.saturating_add(1);
        let halflife = if config.risk_halflife_turns == 0 {
            tracing::warn!("risk_halflife_turns = 0 is invalid, clamping to 1");
            1u32
        } else {
            config.risk_halflife_turns
        };
        let decay = (-std::f64::consts::LN_2 / f64::from(halflife)).exp();
        self.trajectory_risk *= decay;
    }

    /// Ingest a safety signal and add its weighted contribution to `trajectory_risk`.
    ///
    /// The raw score is `base_weight(signal_type) × severity_multiplier(severity)`.
    /// After addition, `trajectory_risk` is clamped to `[0.0, 1.0]`. The event is
    /// appended to the signal history ring buffer; the oldest entry is evicted when
    /// the buffer is full.
    ///
    /// No-op when disabled.
    pub fn ingest(&mut self, signal_type: AuditSignalType, severity: Severity) {
        let _span = info_span!("memory.shadow.ingest").entered();
        let Some(config) = &self.config else { return };

        let base_weight = match signal_type {
            AuditSignalType::PolicyViolation => config.signal_weights.policy_violation,
            AuditSignalType::PromptInjectionPattern => config.signal_weights.prompt_injection,
            AuditSignalType::ToolChainAnomaly => config.signal_weights.tool_chain_anomaly,
            AuditSignalType::ConfidenceDrop => config.signal_weights.confidence_drop,
        };
        let severity_mult = match severity {
            Severity::Low => config.severity_multipliers.low,
            Severity::Medium => config.severity_multipliers.medium,
            Severity::High => config.severity_multipliers.high,
        };
        let raw_score = base_weight * severity_mult;

        self.trajectory_risk = (self.trajectory_risk + raw_score).min(1.0);

        let cap = config.signal_history_cap;
        if self.signal_history.len() >= cap {
            self.signal_history.pop_front();
        }
        self.signal_history.push_back(SignalEvent {
            turn_id: self.turn_count,
            signal_type,
            severity,
            raw_score,
        });
    }

    /// Returns the current accumulated risk score in `[0.0, 1.0]`.
    ///
    /// Always returns `0.0` when disabled.
    #[must_use]
    pub fn current_risk(&self) -> f64 {
        let _span = info_span!("memory.shadow.current_risk").entered();
        if self.config.is_none() {
            return 0.0;
        }
        self.trajectory_risk
    }

    /// Returns `true` when `trajectory_risk >= risk_threshold` and shadow memory is enabled.
    ///
    /// Always returns `false` when disabled.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        let Some(config) = &self.config else {
            return false;
        };
        self.trajectory_risk >= config.risk_threshold
    }

    /// Returns `true` when risk is in `[escalation_threshold, risk_threshold)`.
    ///
    /// Always returns `false` when disabled.
    #[must_use]
    pub fn should_escalate(&self) -> bool {
        let Some(config) = &self.config else {
            return false;
        };
        self.trajectory_risk >= config.escalation_threshold
            && self.trajectory_risk < config.risk_threshold
    }

    /// Returns the top `n` signals by `raw_score` descending from recent history.
    #[must_use]
    pub fn top_signals(&self, n: usize) -> Vec<&SignalEvent> {
        let mut signals: Vec<&SignalEvent> = self.signal_history.iter().collect();
        signals.sort_by(|a, b| {
            b.raw_score
                .partial_cmp(&a.raw_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        signals.truncate(n);
        signals
    }

    /// Resets `trajectory_risk` to zero and clears signal history.
    ///
    /// Called on context compaction when `reset_on_compaction = true`. No-op when disabled.
    pub fn reset(&mut self) {
        if self.config.is_none() {
            return;
        }
        self.trajectory_risk = 0.0;
        self.signal_history.clear();
    }

    /// Returns `true` when shadow memory is enabled (i.e., not in noop mode).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.is_some()
    }

    /// Returns the current turn count.
    #[must_use]
    pub fn turn_count(&self) -> u32 {
        self.turn_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::{
        TrajectoryRiskAccumulatorConfig, TrajectorySeverityMultipliers, TrajectorySignalWeights,
    };

    fn enabled_config() -> TrajectoryRiskAccumulatorConfig {
        TrajectoryRiskAccumulatorConfig {
            enabled: true,
            risk_threshold: 0.75,
            escalation_threshold: 0.50,
            risk_halflife_turns: 10,
            signal_history_cap: 200,
            tui_show_risk_gauge: true,
            reset_on_compaction: false,
            signal_weights: TrajectorySignalWeights::default(),
            severity_multipliers: TrajectorySeverityMultipliers::default(),
        }
    }

    #[test]
    fn new_noop_returns_zero_risk() {
        let acc = TrajectoryRiskAccumulator::new_noop();
        assert!(acc.current_risk() < f64::EPSILON);
        assert!(!acc.is_blocked());
        assert!(!acc.is_enabled());
    }

    #[test]
    fn single_signal_below_threshold_not_blocked() {
        let mut acc = TrajectoryRiskAccumulator::new(enabled_config());
        acc.advance_turn();
        // PolicyViolation medium = 0.30 * 1.0 = 0.30 < 0.75
        acc.ingest(AuditSignalType::PolicyViolation, Severity::Medium);
        assert!(acc.current_risk() > 0.0);
        assert!(acc.current_risk() < 0.75);
        assert!(!acc.is_blocked());
    }

    #[test]
    fn multi_turn_chain_accumulates_and_blocks() {
        let mut acc = TrajectoryRiskAccumulator::new(enabled_config());
        // PromptInjectionPattern high = 0.50 * 2.0 = 1.0 per signal
        // After 2 signals (clamped to 1.0), should be blocked
        for _ in 0..5 {
            acc.advance_turn();
            acc.ingest(AuditSignalType::PromptInjectionPattern, Severity::High);
        }
        assert!(acc.is_blocked(), "risk={}", acc.current_risk());
    }

    #[test]
    fn temporal_decay_reduces_score() {
        let mut acc = TrajectoryRiskAccumulator::new(enabled_config());
        acc.advance_turn();
        acc.ingest(AuditSignalType::PromptInjectionPattern, Severity::High);
        let risk_after_signal = acc.current_risk();
        assert!(risk_after_signal > 0.0);

        // Advance 100 turns without new signals — risk should decay significantly
        for _ in 0..100 {
            acc.advance_turn();
        }
        assert!(
            acc.current_risk() < risk_after_signal / 2.0,
            "expected significant decay, got {}",
            acc.current_risk()
        );
    }

    #[test]
    fn risk_clamped_at_one() {
        let mut acc = TrajectoryRiskAccumulator::new(enabled_config());
        for _ in 0..20 {
            acc.advance_turn();
            acc.ingest(AuditSignalType::PromptInjectionPattern, Severity::High);
        }
        assert!(
            acc.current_risk() <= 1.0,
            "trajectory_risk exceeded 1.0: {}",
            acc.current_risk()
        );
    }

    #[test]
    fn advance_turn_before_ingest_applies_decay() {
        let mut acc = TrajectoryRiskAccumulator::new(enabled_config());
        // Seed some risk first
        acc.advance_turn();
        acc.ingest(AuditSignalType::PolicyViolation, Severity::High);
        let risk_t1 = acc.current_risk();

        // Advance a turn (decay applied) before next ingest
        acc.advance_turn();
        let risk_after_decay = acc.current_risk();

        // After decay, risk should be strictly less than risk_t1 (no new signals yet)
        assert!(
            risk_after_decay < risk_t1,
            "decay should reduce risk before new ingest: {risk_after_decay} vs {risk_t1}"
        );

        acc.ingest(AuditSignalType::PolicyViolation, Severity::High);
        // After ingest, risk should be higher than the decayed value
        assert!(
            acc.current_risk() > risk_after_decay,
            "ingest should increase risk: {} vs {}",
            acc.current_risk(),
            risk_after_decay
        );
    }

    #[test]
    fn decay_formula_matches_spec() {
        // halflife=10, confidence_drop base_weight=0.15, medium severity=1.0
        // 5 turns: each turn calls advance_turn() then ingest(ConfidenceDrop, Medium)
        // per-signal contribution = 0.15 * 1.0 = 0.15; sum over 5 turns < 1.0 so no clamping.
        // After turn 5, the accumulator holds:
        //   risk = 0.15*d^0 + 0.15*d^1 + 0.15*d^2 + 0.15*d^3 + 0.15*d^4
        // where d = exp(-ln(2)/10), most recent signal (turn 5) has least decay (d^0).
        let mut acc = TrajectoryRiskAccumulator::new(enabled_config());
        for _ in 0..5 {
            acc.advance_turn();
            acc.ingest(AuditSignalType::ConfidenceDrop, Severity::Medium);
        }
        let decay = (-std::f64::consts::LN_2 / 10.0_f64).exp();
        // sum_{k=0}^{4} 0.15 * decay^k (most recent turn = k=0, least decay applied)
        let expected: f64 = (0..5).map(|k| 0.15_f64 * decay.powi(k)).sum();
        assert!(
            expected < 1.0,
            "test precondition: expected sum {expected} must be < 1.0 (no clamping)"
        );
        assert!(
            (acc.current_risk() - expected).abs() < 1e-9,
            "expected {expected:.12}, got {:.12}",
            acc.current_risk()
        );
    }

    #[test]
    fn fifty_clean_turns_zero_risk() {
        let mut acc = TrajectoryRiskAccumulator::new(enabled_config());
        for _ in 0..50 {
            acc.advance_turn();
        }
        assert!(
            acc.current_risk() < f64::EPSILON,
            "no signals → risk must stay 0.0"
        );
        assert!(!acc.is_blocked());
    }
}
