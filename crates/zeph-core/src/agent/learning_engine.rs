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
