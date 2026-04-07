// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::config::LearningConfig;

/// Maximum number of concurrent fire-and-forget learning tasks.
///
/// When the `JoinSet` reaches this limit, new spawns are skipped (not aborted) so
/// in-flight work is preserved. The set is detached via `detach_all` at turn boundary.
pub(crate) const MAX_LEARNING_TASKS: usize = 16;

/// Default number of user turns between preference analysis runs.
const DEFAULT_ANALYSIS_INTERVAL: u64 = 5;

/// RL routing configuration snapshot (from `SkillsConfig`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RlRoutingConfig {
    pub(super) enabled: bool,
    pub(super) learning_rate: f32,
    pub(super) persist_interval: u32,
}

pub(crate) struct LearningEngine {
    pub(super) config: Option<LearningConfig>,
    /// RL routing configuration, populated via `with_rl_routing()`.
    pub(super) rl_routing: Option<RlRoutingConfig>,
    pub(super) reflection_used: bool,
    /// Monotonically increasing counter incremented on each user turn.
    turn_counter: u64,
    /// Value of `turn_counter` when the last analysis completed.
    last_analysis_turn: u64,
    /// How many turns to wait between analysis runs.
    analysis_interval: u64,
    /// Highest correction id processed in the last analysis run (watermark).
    /// Stored as `i64` to match `SQLite` row ids; `0` means no analysis has run yet.
    pub(super) last_analyzed_correction_id: i64,
    /// Bounded set of in-flight fire-and-forget learning tasks.
    ///
    /// Capped at `MAX_LEARNING_TASKS`. New spawns are skipped (not aborted) when the
    /// cap is reached. The set is detached via `detach_all` at each turn boundary.
    pub(crate) learning_tasks: tokio::task::JoinSet<()>,
}

impl LearningEngine {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            config: None,
            rl_routing: None,
            reflection_used: false,
            turn_counter: 0,
            last_analysis_turn: 0,
            analysis_interval: DEFAULT_ANALYSIS_INTERVAL,
            last_analyzed_correction_id: 0,
            learning_tasks: tokio::task::JoinSet::new(),
        }
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.config.as_ref().is_some_and(|c| c.enabled)
    }

    /// Returns true when correction analysis should run this turn.
    ///
    /// Gated on `correction_detection` (not `enabled`) so that preference
    /// analysis is independent of skill auto-improvement (S1 from critic).
    pub(super) fn should_analyze(&self) -> bool {
        let Some(cfg) = self.config.as_ref() else {
            return false;
        };
        cfg.correction_detection
            && self.turn_counter >= self.last_analysis_turn + self.analysis_interval
    }

    /// Increment the turn counter. Call once per user message.
    pub(super) fn tick(&mut self) {
        self.turn_counter += 1;
    }

    /// Record that analysis completed at the current turn.
    pub(super) fn mark_analyzed(&mut self) {
        self.last_analysis_turn = self.turn_counter;
    }

    pub(super) fn mark_reflection_used(&mut self) {
        self.reflection_used = true;
    }

    pub(super) fn was_reflection_used(&self) -> bool {
        self.reflection_used
    }

    pub(super) fn reset_reflection(&mut self) {
        self.reflection_used = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults() {
        let e = LearningEngine::new();
        assert!(e.config.is_none());
        assert!(!e.reflection_used);
        assert!(!e.is_enabled());
        assert!(!e.should_analyze());
        assert_eq!(e.last_analyzed_correction_id, 0);
    }

    #[test]
    fn is_enabled_no_config() {
        let e = LearningEngine::new();
        assert!(!e.is_enabled());
    }

    #[test]
    fn is_enabled_disabled_config() {
        let mut e = LearningEngine::new();
        e.config = Some(LearningConfig {
            enabled: false,
            ..Default::default()
        });
        assert!(!e.is_enabled());
    }

    #[test]
    fn is_enabled_enabled_config() {
        let mut e = LearningEngine::new();
        e.config = Some(LearningConfig {
            enabled: true,
            ..Default::default()
        });
        assert!(e.is_enabled());
    }

    #[test]
    fn reflection_lifecycle() {
        let mut e = LearningEngine::new();
        assert!(!e.was_reflection_used());
        e.mark_reflection_used();
        assert!(e.was_reflection_used());
        e.reset_reflection();
        assert!(!e.was_reflection_used());
    }

    #[test]
    fn mark_reflection_idempotent() {
        let mut e = LearningEngine::new();
        e.mark_reflection_used();
        e.mark_reflection_used();
        assert!(e.was_reflection_used());
        e.reset_reflection();
        assert!(!e.was_reflection_used());
    }

    // S1: should_analyze uses correction_detection, not enabled
    #[test]
    fn should_analyze_uses_correction_detection_not_enabled() {
        let mut e = LearningEngine::new();
        // enabled=false, correction_detection=true (default)
        e.config = Some(LearningConfig {
            enabled: false,
            correction_detection: true,
            ..Default::default()
        });
        // Advance enough turns
        for _ in 0..DEFAULT_ANALYSIS_INTERVAL {
            e.tick();
        }
        // Should analyze even though enabled=false
        assert!(e.should_analyze());
    }

    #[test]
    fn should_analyze_false_when_correction_detection_disabled() {
        let mut e = LearningEngine::new();
        e.config = Some(LearningConfig {
            enabled: true,
            correction_detection: false,
            ..Default::default()
        });
        for _ in 0..100 {
            e.tick();
        }
        assert!(!e.should_analyze());
    }

    #[test]
    fn tick_and_analyze_cycle() {
        let mut e = LearningEngine::new();
        e.config = Some(LearningConfig {
            correction_detection: true,
            ..Default::default()
        });
        // Not ready until interval ticks pass
        for i in 0..DEFAULT_ANALYSIS_INTERVAL {
            assert!(!e.should_analyze(), "should not analyze at turn {i}");
            e.tick();
        }
        assert!(e.should_analyze());
        e.mark_analyzed();
        // After marking, should not fire until next interval
        assert!(!e.should_analyze());
        for _ in 0..DEFAULT_ANALYSIS_INTERVAL {
            e.tick();
        }
        assert!(e.should_analyze());
    }
}
