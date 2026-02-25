// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Trust tier enum for skill execution permissions.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Trust tier controlling what a skill is allowed to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
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

impl TrustLevel {
    /// Ordered severity: lower value = more trusted.
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
    #[must_use]
    pub fn min_trust(self, other: Self) -> Self {
        if self.severity() >= other.severity() {
            self
        } else {
            other
        }
    }

    #[must_use]
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Blocked)
    }
}

impl fmt::Display for TrustLevel {
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
        assert!(TrustLevel::Trusted.severity() < TrustLevel::Verified.severity());
        assert!(TrustLevel::Verified.severity() < TrustLevel::Quarantined.severity());
        assert!(TrustLevel::Quarantined.severity() < TrustLevel::Blocked.severity());
    }

    #[test]
    fn min_trust_picks_least_trusted() {
        assert_eq!(
            TrustLevel::Trusted.min_trust(TrustLevel::Quarantined),
            TrustLevel::Quarantined
        );
        assert_eq!(
            TrustLevel::Blocked.min_trust(TrustLevel::Trusted),
            TrustLevel::Blocked
        );
    }

    #[test]
    fn is_active() {
        assert!(TrustLevel::Trusted.is_active());
        assert!(TrustLevel::Verified.is_active());
        assert!(TrustLevel::Quarantined.is_active());
        assert!(!TrustLevel::Blocked.is_active());
    }

    #[test]
    fn default_is_quarantined() {
        assert_eq!(TrustLevel::default(), TrustLevel::Quarantined);
    }

    #[test]
    fn display() {
        assert_eq!(TrustLevel::Trusted.to_string(), "trusted");
        assert_eq!(TrustLevel::Blocked.to_string(), "blocked");
        assert_eq!(TrustLevel::Quarantined.to_string(), "quarantined");
        assert_eq!(TrustLevel::Verified.to_string(), "verified");
    }

    #[test]
    fn serde_roundtrip() {
        let level = TrustLevel::Quarantined;
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, "\"quarantined\"");
        let back: TrustLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(back, level);
    }

    #[test]
    fn serde_all_variants() {
        let cases = [
            (TrustLevel::Trusted, "\"trusted\""),
            (TrustLevel::Verified, "\"verified\""),
            (TrustLevel::Quarantined, "\"quarantined\""),
            (TrustLevel::Blocked, "\"blocked\""),
        ];
        for (level, expected_json) in cases {
            let json = serde_json::to_string(&level).unwrap();
            assert_eq!(json, expected_json);
            let back: TrustLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, level);
        }
    }

    #[test]
    fn min_trust_same_level_returns_self() {
        assert_eq!(
            TrustLevel::Verified.min_trust(TrustLevel::Verified),
            TrustLevel::Verified
        );
        assert_eq!(
            TrustLevel::Blocked.min_trust(TrustLevel::Blocked),
            TrustLevel::Blocked
        );
    }

    #[test]
    fn hash_consistent() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(TrustLevel::Trusted);
        set.insert(TrustLevel::Verified);
        set.insert(TrustLevel::Quarantined);
        set.insert(TrustLevel::Blocked);
        assert_eq!(set.len(), 4);
        // Inserting same value again does not grow the set
        set.insert(TrustLevel::Trusted);
        assert_eq!(set.len(), 4);
    }
}
