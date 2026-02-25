// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::config::LearningConfig;

pub(crate) struct LearningEngine {
    pub(super) config: Option<LearningConfig>,
    pub(super) reflection_used: bool,
}

impl LearningEngine {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            config: None,
            reflection_used: false,
        }
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.config.as_ref().is_some_and(|c| c.enabled)
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
}
