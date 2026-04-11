// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Trust tier enum for skill execution permissions.
//!
//! [`SkillTrustLevel`] is the single source of truth for trust-level semantics across all
//! Zeph crates. It lives in `zeph-common` so both `zeph-skills` and `zeph-tools` can depend
//! on it without introducing a circular dependency.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Trust tier controlling what a skill is allowed to do.
///
/// The ordering from most to least trusted is: `Trusted` → `Verified` → `Quarantined` →
/// `Blocked`. Use [`SkillTrustLevel::severity`] to compare levels numerically, or
/// [`SkillTrustLevel::min_trust`] to find the least-trusted of two levels.
///
/// # Examples
///
/// ```rust
/// use zeph_common::SkillTrustLevel;
///
/// let level = SkillTrustLevel::Quarantined;
/// assert!(level.is_active());
/// assert_eq!(level.severity(), 2);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillTrustLevel {
    /// Built-in or user-audited skill: full tool access.
    Trusted,
    /// Signature or hash verified: default tool access.
    Verified,
    /// Newly imported or hash-mismatch: restricted tool access.
    #[default]
    Quarantined,
    /// Explicitly disabled by user or auto-blocked by anomaly detector.
    Blocked,
}

impl SkillTrustLevel {
    /// Ordered severity: lower value = more trusted.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_common::SkillTrustLevel;
    ///
    /// assert!(SkillTrustLevel::Trusted.severity() < SkillTrustLevel::Blocked.severity());
    /// ```
    #[must_use]
    pub fn severity(self) -> u8 {
        match self {
            Self::Trusted => 0,
            Self::Verified => 1,
            Self::Quarantined => 2,
            Self::Blocked => 3,
        }
    }

    /// Returns the least-trusted (highest severity) of two levels.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_common::SkillTrustLevel;
    ///
    /// let result = SkillTrustLevel::Trusted.min_trust(SkillTrustLevel::Quarantined);
    /// assert_eq!(result, SkillTrustLevel::Quarantined);
    /// ```
    #[must_use]
    pub fn min_trust(self, other: Self) -> Self {
        if self.severity() >= other.severity() {
            self
        } else {
            other
        }
    }

    /// Returns `true` if the level is not `Blocked`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_common::SkillTrustLevel;
    ///
    /// assert!(SkillTrustLevel::Quarantined.is_active());
    /// assert!(!SkillTrustLevel::Blocked.is_active());
    /// ```
    #[must_use]
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Blocked)
    }
}

impl FromStr for SkillTrustLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "trusted" => Ok(Self::Trusted),
            "verified" => Ok(Self::Verified),
            "quarantined" => Ok(Self::Quarantined),
            "blocked" => Ok(Self::Blocked),
            other => Err(format!(
                "unknown trust level '{other}'; expected: trusted, verified, quarantined, blocked"
            )),
        }
    }
}

impl fmt::Display for SkillTrustLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Trusted => f.write_str("trusted"),
            Self::Verified => f.write_str("verified"),
            Self::Quarantined => f.write_str("quarantined"),
            Self::Blocked => f.write_str("blocked"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_ordering() {
        assert!(SkillTrustLevel::Trusted.severity() < SkillTrustLevel::Verified.severity());
        assert!(SkillTrustLevel::Verified.severity() < SkillTrustLevel::Quarantined.severity());
        assert!(SkillTrustLevel::Quarantined.severity() < SkillTrustLevel::Blocked.severity());
    }

    #[test]
    fn min_trust_picks_least_trusted() {
        assert_eq!(
            SkillTrustLevel::Trusted.min_trust(SkillTrustLevel::Quarantined),
            SkillTrustLevel::Quarantined
        );
        assert_eq!(
            SkillTrustLevel::Blocked.min_trust(SkillTrustLevel::Trusted),
            SkillTrustLevel::Blocked
        );
    }

    #[test]
    fn is_active() {
        assert!(SkillTrustLevel::Trusted.is_active());
        assert!(SkillTrustLevel::Verified.is_active());
        assert!(SkillTrustLevel::Quarantined.is_active());
        assert!(!SkillTrustLevel::Blocked.is_active());
    }

    #[test]
    fn default_is_quarantined() {
        assert_eq!(SkillTrustLevel::default(), SkillTrustLevel::Quarantined);
    }

    #[test]
    fn display() {
        assert_eq!(SkillTrustLevel::Trusted.to_string(), "trusted");
        assert_eq!(SkillTrustLevel::Blocked.to_string(), "blocked");
        assert_eq!(SkillTrustLevel::Quarantined.to_string(), "quarantined");
        assert_eq!(SkillTrustLevel::Verified.to_string(), "verified");
    }

    #[test]
    fn serde_roundtrip() {
        let level = SkillTrustLevel::Quarantined;
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, "\"quarantined\"");
        let back: SkillTrustLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(back, level);
    }

    #[test]
    fn min_trust_same_level_returns_self() {
        assert_eq!(
            SkillTrustLevel::Verified.min_trust(SkillTrustLevel::Verified),
            SkillTrustLevel::Verified
        );
    }
}
