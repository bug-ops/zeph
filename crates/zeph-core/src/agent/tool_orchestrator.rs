// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::VecDeque;

use zeph_tools::OverflowConfig;

use super::DOOM_LOOP_WINDOW;

pub(crate) struct ToolOrchestrator {
    pub(super) doom_loop_history: Vec<u64>,
    pub(super) max_iterations: usize,
    pub(super) summarize_tool_output_enabled: bool,
    pub(super) overflow_config: OverflowConfig,
    /// Sliding window of recent (`tool_name`, `args_hash`) pairs for repeat-detection.
    /// Only LLM-initiated calls are recorded here — retry re-executions are excluded.
    /// Window capacity = 2 * `repeat_threshold`.
    pub(super) recent_tool_calls: VecDeque<(String, u64)>,
    /// Number of identical (`tool_name`, `args_hash`) appearances in the window required
    /// to trigger repeat-detection abort. 0 = disabled.
    pub(super) repeat_threshold: usize,
    /// Max retries for transient errors per tool call. 0 = disabled.
    pub(super) max_tool_retries: usize,
}

impl ToolOrchestrator {
    #[must_use]
    pub(crate) fn new() -> Self {
        let repeat_threshold = 2_usize;
        Self {
            doom_loop_history: Vec::new(),
            max_iterations: 10,
            summarize_tool_output_enabled: false,
            overflow_config: OverflowConfig::default(),
            recent_tool_calls: VecDeque::with_capacity(2 * repeat_threshold),
            repeat_threshold,
            max_tool_retries: 2,
        }
    }

    pub(super) fn push_doom_hash(&mut self, hash: u64) {
        self.doom_loop_history.push(hash);
    }

    pub(super) fn clear_doom_history(&mut self) {
        self.doom_loop_history.clear();
    }

    /// Reset the repeat-detection sliding window between user turns.
    pub(super) fn clear_recent_tool_calls(&mut self) {
        self.recent_tool_calls.clear();
    }

    /// Returns `true` if the last `DOOM_LOOP_WINDOW` hashes are identical.
    pub(super) fn is_doom_loop(&self) -> bool {
        if self.doom_loop_history.len() < DOOM_LOOP_WINDOW {
            return false;
        }
        let recent = &self.doom_loop_history[self.doom_loop_history.len() - DOOM_LOOP_WINDOW..];
        recent.windows(2).all(|w| w[0] == w[1])
    }

    /// Record a tool call (LLM-initiated only — not retry re-executions).
    ///
    /// Maintains a sliding window of size `2 * repeat_threshold`.
    pub(super) fn push_tool_call(&mut self, name: &str, args_hash: u64) {
        if self.repeat_threshold == 0 {
            return;
        }
        let window = 2 * self.repeat_threshold;
        if self.recent_tool_calls.len() >= window {
            self.recent_tool_calls.pop_front();
        }
        self.recent_tool_calls
            .push_back((name.to_owned(), args_hash));
    }

    /// Returns `true` if the same `(name, args_hash)` pair appears `>= repeat_threshold`
    /// times in the current window.
    pub(super) fn is_repeat(&self, name: &str, args_hash: u64) -> bool {
        if self.repeat_threshold == 0 {
            return false;
        }
        let count = self
            .recent_tool_calls
            .iter()
            .filter(|(n, h)| n == name && *h == args_hash)
            .count();
        count >= self.repeat_threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults() {
        let o = ToolOrchestrator::new();
        assert!(o.doom_loop_history.is_empty());
        assert_eq!(o.max_iterations, 10);
        assert!(!o.summarize_tool_output_enabled);
        assert!(o.recent_tool_calls.is_empty());
        assert_eq!(o.repeat_threshold, 2);
        assert_eq!(o.max_tool_retries, 2);
    }

    #[test]
    fn is_doom_loop_insufficient_history() {
        let mut o = ToolOrchestrator::new();
        o.push_doom_hash(42);
        o.push_doom_hash(42);
        assert!(!o.is_doom_loop());
    }

    #[test]
    fn is_doom_loop_identical_hashes() {
        let mut o = ToolOrchestrator::new();
        o.push_doom_hash(7);
        o.push_doom_hash(7);
        o.push_doom_hash(7);
        assert!(o.is_doom_loop());
    }

    #[test]
    fn is_doom_loop_mixed_hashes() {
        let mut o = ToolOrchestrator::new();
        o.push_doom_hash(1);
        o.push_doom_hash(2);
        o.push_doom_hash(3);
        assert!(!o.is_doom_loop());
    }

    #[test]
    fn is_doom_loop_only_recent_window_matters() {
        let mut o = ToolOrchestrator::new();
        o.push_doom_hash(1);
        o.push_doom_hash(2);
        o.push_doom_hash(9);
        o.push_doom_hash(9);
        o.push_doom_hash(9);
        assert!(o.is_doom_loop());
    }

    #[test]
    fn clear_doom_history_resets() {
        let mut o = ToolOrchestrator::new();
        o.push_doom_hash(5);
        o.push_doom_hash(5);
        o.push_doom_hash(5);
        assert!(o.is_doom_loop());
        o.clear_doom_history();
        assert!(!o.is_doom_loop());
        assert!(o.doom_loop_history.is_empty());
    }

    // Repeat-detection tests

    #[test]
    fn repeat_detection_no_repeat_before_threshold() {
        let mut o = ToolOrchestrator::new();
        o.push_tool_call("bash", 42);
        // Only 1 occurrence, threshold is 2 — not a repeat yet
        assert!(!o.is_repeat("bash", 42));
    }

    #[test]
    fn repeat_detection_triggers_at_threshold() {
        let mut o = ToolOrchestrator::new();
        o.push_tool_call("bash", 42);
        o.push_tool_call("bash", 42);
        // 2 occurrences >= threshold 2 → repeat
        assert!(o.is_repeat("bash", 42));
    }

    #[test]
    fn repeat_detection_different_args_no_repeat() {
        let mut o = ToolOrchestrator::new();
        o.push_tool_call("bash", 1);
        o.push_tool_call("bash", 2);
        assert!(!o.is_repeat("bash", 1));
        assert!(!o.is_repeat("bash", 2));
    }

    #[test]
    fn repeat_detection_different_tool_no_repeat() {
        let mut o = ToolOrchestrator::new();
        o.push_tool_call("bash", 42);
        o.push_tool_call("read", 42);
        assert!(!o.is_repeat("bash", 42));
        assert!(!o.is_repeat("read", 42));
    }

    #[test]
    fn repeat_detection_window_evicts_old_entries() {
        let mut o = ToolOrchestrator::new();
        // Window size = 2 * threshold = 4
        o.push_tool_call("bash", 42);
        o.push_tool_call("read", 1);
        o.push_tool_call("read", 2);
        o.push_tool_call("read", 3);
        // Now push another entry — "bash:42" should be evicted from front
        o.push_tool_call("read", 4);
        // "bash:42" was in the window once, now evicted → not a repeat
        assert!(!o.is_repeat("bash", 42));
    }

    #[test]
    fn repeat_detection_disabled_when_threshold_zero() {
        let mut o = ToolOrchestrator::new();
        o.repeat_threshold = 0;
        o.push_tool_call("bash", 42);
        o.push_tool_call("bash", 42);
        o.push_tool_call("bash", 42);
        // Threshold 0 means disabled
        assert!(!o.is_repeat("bash", 42));
    }
}
