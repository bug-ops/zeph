//! Skill trust levels and source tracking.

use std::fmt;
use std::path::{Path, PathBuf};

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

/// Where a skill was loaded from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SkillSource {
    /// Built-in skill shipped with the binary.
    #[default]
    Local,
    /// Downloaded from a skill hub.
    Hub { url: String },
    /// Imported from a local file path.
    File { path: PathBuf },
}

impl fmt::Display for SkillSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => f.write_str("local"),
            Self::Hub { url } => write!(f, "hub({url})"),
            Self::File { path } => write!(f, "file({})", path.display()),
        }
    }
}

/// Trust metadata attached to a loaded skill.
#[derive(Debug, Clone)]
pub struct SkillTrust {
    pub skill_name: String,
    pub trust_level: TrustLevel,
    pub source: SkillSource,
    pub blake3_hash: String,
}

/// Compute blake3 hash of a SKILL.md file.
///
/// # Errors
///
/// Returns an IO error if the file cannot be read.
pub fn compute_skill_hash(skill_dir: &Path) -> std::io::Result<String> {
    let content = std::fs::read(skill_dir.join("SKILL.md"))?;
    Ok(blake3::hash(&content).to_hex().to_string())
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
        assert_eq!(SkillSource::Local.to_string(), "local");
        assert_eq!(
            SkillSource::Hub {
                url: "https://example.com".into()
            }
            .to_string(),
            "hub(https://example.com)"
        );
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
    fn compute_hash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "test content").unwrap();
        let hash = compute_skill_hash(dir.path()).unwrap();
        assert_eq!(hash.len(), 64); // blake3 hex is 64 chars
        // Same content = same hash
        let hash2 = compute_skill_hash(dir.path()).unwrap();
        assert_eq!(hash, hash2);
    }

    #[test]
    fn compute_hash_different_content() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(dir1.path().join("SKILL.md"), "content a").unwrap();
        std::fs::write(dir2.path().join("SKILL.md"), "content b").unwrap();
        let h1 = compute_skill_hash(dir1.path()).unwrap();
        let h2 = compute_skill_hash(dir2.path()).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn source_serde_roundtrip() {
        let source = SkillSource::Hub {
            url: "https://hub.example.com/skill".into(),
        };
        let json = serde_json::to_string(&source).unwrap();
        let back: SkillSource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, source);
    }
}
