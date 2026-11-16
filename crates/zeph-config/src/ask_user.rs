// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration for the model-callable `ask_user` tool (`[tools.ask_user]`).

use std::num::{NonZeroU8, NonZeroU64};

use serde::{Deserialize, Serialize};

fn default_timeout_secs() -> NonZeroU64 {
    NonZeroU64::new(300).expect("300 is non-zero")
}

fn default_max_questions_per_turn() -> NonZeroU8 {
    NonZeroU8::new(3).expect("3 is non-zero")
}

/// `[tools.ask_user]`: lets the model ask the user a clarifying question mid-turn.
///
/// The tool is opt-in: it is only offered to the model when `enabled` is `true`. Both numeric
/// limits are non-zero by construction, so a zero timeout or question cap cannot be configured.
///
/// # Examples
///
/// ```
/// use zeph_config::AskUserConfig;
///
/// let cfg = AskUserConfig::default();
/// assert!(!cfg.enabled);
/// assert_eq!(cfg.timeout_secs.get(), 300);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct AskUserConfig {
    /// Offer the `ask_user` tool to the model. Default: `false`.
    pub enabled: bool,
    /// Seconds to wait for the user's answer before the question times out. Default: `300`.
    pub timeout_secs: NonZeroU64,
    /// Maximum questions the model may ask in a single turn. Default: `3`.
    pub max_questions_per_turn: NonZeroU8,
}

impl Default for AskUserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_secs: default_timeout_secs(),
            max_questions_per_turn: default_max_questions_per_turn(),
        }
    }
}
