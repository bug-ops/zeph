// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration for the model-callable `ask_user` tool (`[tools.ask_user]`).

use std::num::NonZeroU8;

use serde::{Deserialize, Serialize};

/// Error returned when an `ask_user` timeout is outside `1..=3600` seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("ask_user timeout {0}s is outside 1..=3600 seconds")]
pub struct AskUserTimeoutError(u64);

/// Seconds to wait for the user's answer to an `ask_user` question, in `1..=3600`.
///
/// A zero timeout would expire every question immediately and an unbounded one would park the
/// turn indefinitely, so both are rejected at construction and at deserialization.
///
/// # Examples
///
/// ```
/// use zeph_config::AskUserTimeout;
///
/// assert_eq!(AskUserTimeout::new(300).unwrap().get(), 300);
/// assert!(AskUserTimeout::new(0).is_err());
/// assert!(AskUserTimeout::new(3601).is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct AskUserTimeout(u64);

impl AskUserTimeout {
    /// Smallest accepted timeout in seconds.
    pub const MIN_SECS: u64 = 1;
    /// Largest accepted timeout in seconds.
    pub const MAX_SECS: u64 = 3600;

    /// Build a timeout from a number of seconds.
    ///
    /// # Errors
    ///
    /// Returns [`AskUserTimeoutError`] when `secs` is outside `1..=3600`.
    pub const fn new(secs: u64) -> Result<Self, AskUserTimeoutError> {
        if secs < Self::MIN_SECS || secs > Self::MAX_SECS {
            Err(AskUserTimeoutError(secs))
        } else {
            Ok(Self(secs))
        }
    }

    /// The timeout in seconds.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for AskUserTimeout {
    type Error = AskUserTimeoutError;

    fn try_from(secs: u64) -> Result<Self, Self::Error> {
        Self::new(secs)
    }
}

impl From<AskUserTimeout> for u64 {
    fn from(timeout: AskUserTimeout) -> Self {
        timeout.0
    }
}

impl Default for AskUserTimeout {
    fn default() -> Self {
        Self(300)
    }
}

fn default_max_questions_per_turn() -> NonZeroU8 {
    NonZeroU8::new(3).expect("3 is non-zero")
}

/// `[tools.ask_user]`: lets the model ask the user a clarifying question mid-turn.
///
/// The tool is opt-in: it is only offered to the model when `enabled` is `true`. The timeout is
/// bounded to `1..=3600` seconds and the question cap is non-zero by construction.
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
    /// Seconds to wait for the user's answer before the question times out, `1..=3600`. Default: `300`.
    pub timeout_secs: AskUserTimeout,
    /// Maximum questions the model may ask in a single turn. Default: `3`.
    pub max_questions_per_turn: NonZeroU8,
}

impl Default for AskUserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_secs: AskUserTimeout::default(),
            max_questions_per_turn: default_max_questions_per_turn(),
        }
    }
}
