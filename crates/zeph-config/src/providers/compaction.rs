// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Validated trigger for Claude server-side context compaction.

use serde::{Deserialize, Serialize};

/// Smallest `input_tokens` trigger the Anthropic compaction API accepts.
const MIN_TRIGGER_TOKENS: u32 = 50_000;

/// Error returned when a compaction trigger is below the API minimum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("compaction trigger {0} is below the API minimum of {MIN_TRIGGER_TOKENS} input tokens")]
pub struct CompactionTriggerError(u32);

/// Input-token count at which Claude server-side compaction fires.
///
/// The Anthropic `compact_20260112` edit rejects triggers below 50 000 tokens with a 400, so the
/// invariant is enforced at construction and at deserialization: an invalid value from
/// `server_compaction_trigger_tokens` fails config loading instead of every request.
///
/// # Examples
///
/// ```
/// use zeph_config::CompactionTrigger;
///
/// assert!(CompactionTrigger::new(49_999).is_err());
/// assert_eq!(CompactionTrigger::new(140_000).unwrap().get(), 140_000);
/// assert_eq!(CompactionTrigger::at_least_min(10).get(), 50_000);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct CompactionTrigger(u32);

impl CompactionTrigger {
    /// The smallest accepted trigger (50 000 input tokens).
    pub const MIN: Self = Self(MIN_TRIGGER_TOKENS);

    /// Build a trigger from a token count.
    ///
    /// # Errors
    ///
    /// Returns [`CompactionTriggerError`] when `tokens` is below 50 000.
    pub const fn new(tokens: u32) -> Result<Self, CompactionTriggerError> {
        if tokens < MIN_TRIGGER_TOKENS {
            Err(CompactionTriggerError(tokens))
        } else {
            Ok(Self(tokens))
        }
    }

    /// Build a trigger from a derived token count, raising it to the API minimum when lower.
    #[must_use]
    pub const fn at_least_min(tokens: u32) -> Self {
        if tokens < MIN_TRIGGER_TOKENS {
            Self::MIN
        } else {
            Self(tokens)
        }
    }

    /// The trigger used when none is configured: 70% of `window` input tokens, raised to the
    /// API minimum.
    #[must_use]
    pub const fn default_for_window(window: u32) -> Self {
        Self::at_least_min(window / 10 * 7)
    }

    /// The trigger in input tokens.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for CompactionTrigger {
    type Error = CompactionTriggerError;

    fn try_from(tokens: u32) -> Result<Self, Self::Error> {
        Self::new(tokens)
    }
}

impl From<CompactionTrigger> for u32 {
    fn from(trigger: CompactionTrigger) -> Self {
        trigger.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_below_minimum() {
        assert_eq!(
            CompactionTrigger::new(49_999),
            Err(CompactionTriggerError(49_999))
        );
    }

    #[test]
    fn accepts_minimum() {
        assert_eq!(CompactionTrigger::new(50_000), Ok(CompactionTrigger::MIN));
    }

    #[test]
    fn at_least_min_clamps() {
        assert_eq!(CompactionTrigger::at_least_min(1), CompactionTrigger::MIN);
        assert_eq!(CompactionTrigger::at_least_min(70_000).get(), 70_000);
        assert_eq!(
            CompactionTrigger::default_for_window(200_000).get(),
            140_000
        );
        assert_eq!(CompactionTrigger::default_for_window(40_000).get(), 50_000);
    }

    #[test]
    fn deserialization_enforces_minimum() {
        #[derive(Deserialize)]
        struct Holder {
            trigger: CompactionTrigger,
        }
        assert!(toml::from_str::<Holder>("trigger = 49999").is_err());
        let ok: Holder = toml::from_str("trigger = 120000").unwrap();
        assert_eq!(ok.trigger.get(), 120_000);
    }
}
