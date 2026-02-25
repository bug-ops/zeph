// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Skill trust levels and source tracking.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
pub use zeph_tools::TrustLevel;

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
    fn display() {
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

    #[test]
    fn display_file_source() {
        let source = SkillSource::File {
            path: std::path::PathBuf::from("/tmp/my-skill"),
        };
        assert_eq!(source.to_string(), "file(/tmp/my-skill)");
    }

    #[test]
    fn display_local_source() {
        assert_eq!(SkillSource::Local.to_string(), "local");
    }

    #[test]
    fn compute_hash_missing_skill_md_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        // No SKILL.md written — expect IO error
        let result = compute_skill_hash(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn trust_level_reexport_accessible() {
        // Ensure TrustLevel re-exported from zeph-tools is usable
        let level: TrustLevel = TrustLevel::default();
        assert_eq!(level, TrustLevel::Quarantined);
        assert!(level.is_active());
    }

    #[test]
    fn source_default_is_local() {
        assert_eq!(SkillSource::default(), SkillSource::Local);
    }

    #[test]
    fn source_file_serde_roundtrip() {
        let source = SkillSource::File {
            path: std::path::PathBuf::from("/skills/my_skill"),
        };
        let json = serde_json::to_string(&source).unwrap();
        let back: SkillSource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, source);
    }
}
