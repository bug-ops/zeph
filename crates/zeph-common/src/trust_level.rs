// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Trust tier enums shared across Zeph crates.
//!
//! [`SkillTrustLevel`] is the single source of truth for skill execution permissions.
//! [`TurnContextTrust`] is the content-trust tier of per-turn context and of recalled graph
//! facts. Both live in `zeph-common` so crates such as `zeph-skills`, `zeph-tools`,
//! `zeph-llm` and `zeph-memory` can share them without circular dependencies.

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
#[non_exhaustive]
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
    /// Trust level to assume when a skill has no entry in the trust map.
    ///
    /// A missing entry means "never classified yet" (e.g. persistence not wired, or a
    /// transient trust-map read failure), not "known untrusted" — callers must not fall
    /// back to [`SkillTrustLevel::default`] ([`Quarantined`](Self::Quarantined)) for this
    /// case, as that would misclassify legitimately trusted, already-vetted skills.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_common::SkillTrustLevel;
    ///
    /// let trust_levels: std::collections::HashMap<String, SkillTrustLevel> =
    ///     std::collections::HashMap::new();
    /// let trust = trust_levels
    ///     .get("some-skill")
    ///     .copied()
    ///     .unwrap_or(SkillTrustLevel::MISSING_ENTRY_FALLBACK);
    /// assert_eq!(trust, SkillTrustLevel::Trusted);
    /// ```
    pub const MISSING_ENTRY_FALLBACK: Self = Self::Trusted;

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
    pub const fn severity(self) -> u8 {
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
    pub const fn min_trust(self, other: Self) -> Self {
        if self.severity() >= other.severity() {
            self
        } else {
            other
        }
    }

    /// Inverse of [`severity`](Self::severity): reconstructs a level from its ordinal.
    ///
    /// Any value `>= 3` maps to [`Blocked`](Self::Blocked) — the most restrictive level —
    /// so a corrupted or out-of-range stored ordinal fails closed rather than open.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_common::SkillTrustLevel;
    ///
    /// assert_eq!(SkillTrustLevel::from_severity(0), SkillTrustLevel::Trusted);
    /// assert_eq!(SkillTrustLevel::from_severity(3), SkillTrustLevel::Blocked);
    /// assert_eq!(SkillTrustLevel::from_severity(255), SkillTrustLevel::Blocked);
    /// ```
    #[must_use]
    pub const fn from_severity(v: u8) -> Self {
        match v {
            0 => Self::Trusted,
            1 => Self::Verified,
            2 => Self::Quarantined,
            _ => Self::Blocked,
        }
    }

    /// Returns the string representation used for database storage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Verified => "verified",
            Self::Quarantined => "quarantined",
            Self::Blocked => "blocked",
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
    pub const fn is_active(self) -> bool {
        !matches!(self, Self::Blocked)
    }

    /// Returns `true` for trust levels that must never appear on a listing surface with no
    /// trust-annotation mechanism — [`Blocked`](Self::Blocked) (explicitly disabled) and
    /// [`Quarantined`](Self::Quarantined) (unreviewed / hash-mismatched) alike.
    ///
    /// This is the *hide* strategy for surfaces that cannot show a trust level inline, e.g.
    /// the mention-picker catalog (`SkillCatalogItem` carries only `name`/`description`, no
    /// trust field). Contrast with the *annotate* strategy used by the XML skill-prompt
    /// catalog (`zeph_skills::prompt::format_skills_catalog`'s `trust_levels` parameter),
    /// which keeps a `Quarantined`/`Blocked` skill visible with a `trust="..."` attribute so
    /// the model/operator can still name it and promote it — that surface intentionally does
    /// *not* use this method. This method says nothing about matching-candidate selection or
    /// per-turn dispatch gating (`TurnTrustFloor`, weakest-link trust fold) either — those are
    /// separate concerns with their own filtering logic.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_common::SkillTrustLevel;
    ///
    /// assert!(SkillTrustLevel::Blocked.is_hidden_from_catalog());
    /// assert!(SkillTrustLevel::Quarantined.is_hidden_from_catalog());
    /// assert!(!SkillTrustLevel::Trusted.is_hidden_from_catalog());
    /// ```
    #[must_use]
    pub const fn is_hidden_from_catalog(self) -> bool {
        matches!(self, Self::Blocked | Self::Quarantined)
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

/// Content-trust tier of a per-turn context block (the minimum over its sections) and of
/// recalled graph facts.
///
/// This is the single trust-tier enum; `zeph_sanitizer::ContentTrustLevel` is an alias of it.
/// The enum is exhaustive: adding a tier is a deliberate breaking change that forces every
/// `match` to be revisited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum TurnContextTrust {
    /// System-authored or direct user content.
    Trusted = 0,
    /// Output of local executors.
    LocalUntrusted = 1,
    /// External sources: web, MCP, A2A, memory retrieval.
    ExternalUntrusted = 2,
}

impl TurnContextTrust {
    /// The `u8` discriminant.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::trust_level::TurnContextTrust;
    ///
    /// assert_eq!(TurnContextTrust::ExternalUntrusted.as_u8(), 2);
    /// ```
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`as_u8`](Self::as_u8); `None` for an unknown discriminant.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::trust_level::TurnContextTrust;
    ///
    /// assert_eq!(TurnContextTrust::from_u8(1), Some(TurnContextTrust::LocalUntrusted));
    /// assert_eq!(TurnContextTrust::from_u8(9), None);
    /// ```
    #[must_use]
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Trusted),
            1 => Some(Self::LocalUntrusted),
            2 => Some(Self::ExternalUntrusted),
            _ => None,
        }
    }

    /// Like [`from_u8`](Self::from_u8) but fails closed: an unknown discriminant is treated as
    /// the least trusted tier, so it can never gain system authority.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::trust_level::TurnContextTrust;
    ///
    /// assert_eq!(TurnContextTrust::from_u8_or_untrusted(0), TurnContextTrust::Trusted);
    /// assert_eq!(TurnContextTrust::from_u8_or_untrusted(200), TurnContextTrust::ExternalUntrusted);
    /// ```
    #[must_use]
    pub fn from_u8_or_untrusted(value: u8) -> Self {
        Self::from_u8(value).unwrap_or(Self::ExternalUntrusted)
    }

    /// Parse the `snake_case` identifier produced by [`as_str`](Self::as_str).
    ///
    /// Returns `None` for unrecognized strings so callers can fall back to a conservative
    /// default and log a warning instead of failing deserialization.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::trust_level::TurnContextTrust;
    ///
    /// assert_eq!(
    ///     TurnContextTrust::from_str_opt("local_untrusted"),
    ///     Some(TurnContextTrust::LocalUntrusted)
    /// );
    /// assert_eq!(TurnContextTrust::from_str_opt("unknown"), None);
    /// ```
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "trusted" => Some(Self::Trusted),
            "local_untrusted" => Some(Self::LocalUntrusted),
            "external_untrusted" => Some(Self::ExternalUntrusted),
            _ => None,
        }
    }

    /// Lowercase `snake_case` name, matching the serde representation.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::trust_level::TurnContextTrust;
    ///
    /// assert_eq!(TurnContextTrust::LocalUntrusted.as_str(), "local_untrusted");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::LocalUntrusted => "local_untrusted",
            Self::ExternalUntrusted => "external_untrusted",
        }
    }
}

/// Format version of a sub-agent transcript with respect to per-result trust tagging.
///
/// A transcript written before per-result trust markers existed (`Legacy`) carries no
/// information about where its tool output came from, so it is imported at
/// `ExternalUntrusted`. A `PerResult` transcript tags each tool result and is trusted
/// according to the maximum per-result trust (with a `LocalUntrusted` floor).
///
/// There is deliberately no `Default`: callers must choose a schema explicitly. Serde
/// deserialization of metadata that lacks the field uses [`TranscriptTrustSchema::legacy`].
///
/// # Examples
///
/// ```rust
/// use zeph_common::trust_level::TranscriptTrustSchema;
///
/// assert_eq!(serde_json::to_string(&TranscriptTrustSchema::Legacy).unwrap(), "\"legacy\"");
/// let back: TranscriptTrustSchema = serde_json::from_str("\"per_result\"").unwrap();
/// assert_eq!(back, TranscriptTrustSchema::PerResult);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptTrustSchema {
    /// Written before per-result trust tagging; imports at `ExternalUntrusted`.
    Legacy,
    /// Every tool result carries its own trust marker.
    PerResult,
}

impl TranscriptTrustSchema {
    /// Serde default for metadata files that predate the `trust_schema` field.
    #[must_use]
    pub const fn legacy() -> Self {
        Self::Legacy
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
    fn is_hidden_from_catalog_excludes_blocked_and_quarantined_only() {
        assert!(SkillTrustLevel::Blocked.is_hidden_from_catalog());
        assert!(SkillTrustLevel::Quarantined.is_hidden_from_catalog());
        assert!(!SkillTrustLevel::Trusted.is_hidden_from_catalog());
        assert!(!SkillTrustLevel::Verified.is_hidden_from_catalog());
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

    #[test]
    fn from_severity_round_trips_through_severity() {
        for level in [
            SkillTrustLevel::Trusted,
            SkillTrustLevel::Verified,
            SkillTrustLevel::Quarantined,
            SkillTrustLevel::Blocked,
        ] {
            assert_eq!(SkillTrustLevel::from_severity(level.severity()), level);
        }
    }

    #[test]
    fn from_severity_out_of_range_fails_closed_to_blocked() {
        assert_eq!(SkillTrustLevel::from_severity(4), SkillTrustLevel::Blocked);
        assert_eq!(
            SkillTrustLevel::from_severity(255),
            SkillTrustLevel::Blocked
        );
    }

    #[test]
    fn transcript_trust_schema_serde_roundtrip() {
        for (v, text) in [
            (TranscriptTrustSchema::Legacy, "\"legacy\""),
            (TranscriptTrustSchema::PerResult, "\"per_result\""),
        ] {
            assert_eq!(serde_json::to_string(&v).unwrap(), text);
            assert_eq!(
                serde_json::from_str::<TranscriptTrustSchema>(text).unwrap(),
                v
            );
        }
        assert_eq!(
            TranscriptTrustSchema::legacy(),
            TranscriptTrustSchema::Legacy
        );
    }
}
