// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_tools::OverflowConfig;

use super::DOOM_LOOP_WINDOW;

pub(crate) struct ToolOrchestrator {
    pub(super) doom_loop_history: Vec<u64>,
    pub(super) max_iterations: usize,
    pub(super) summarize_tool_output_enabled: bool,
    pub(super) overflow_config: OverflowConfig,
}

impl ToolOrchestrator {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            doom_loop_history: Vec::new(),
            max_iterations: 10,
            summarize_tool_output_enabled: false,
            overflow_config: OverflowConfig::default(),
        }
    }

    pub(super) fn push_doom_hash(&mut self, hash: u64) {
        self.doom_loop_history.push(hash);
    }

    pub(super) fn clear_doom_history(&mut self) {
        self.doom_loop_history.clear();
    }

    /// Returns `true` if the last `DOOM_LOOP_WINDOW` hashes are identical.
    pub(super) fn is_doom_loop(&self) -> bool {
        if self.doom_loop_history.len() < DOOM_LOOP_WINDOW {
            return false;
        }
        let recent = &self.doom_loop_history[self.doom_loop_history.len() - DOOM_LOOP_WINDOW..];
        recent.windows(2).all(|w| w[0] == w[1])
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
}
