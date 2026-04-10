// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Skill trust levels and provenance tracking.
//!
//! Each installed skill has an associated [`SkillTrust`] record stored in the trust database
//! by `zeph-core`. The record pairs a [`SkillTrustLevel`] (which gates tool access) with a
//! [`SkillSource`] (where the skill came from) and a blake3 content hash (for integrity
//! verification).
//!
//! # Trust Levels (re-exported from `zeph-tools`)
//!
//! | Level | Tool access | When to use |
//! |-------|-------------|-------------|
//! | `Trusted` | Unrestricted | Bundled skills vetted by the maintainer |
//! | `Verified` | Unrestricted | User-approved skills from known sources |
//! | `Quarantined` | Read-only subset | Skills installed but not yet reviewed |
//! | `Blocked` | No tools | Skills flagged for removal |

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
pub use zeph_tools::SkillTrustLevel;

/// Provenance record for an installed skill.
///
/// Serialized with an inline `"kind"` tag for compact JSON storage.
///
/// # Examples
///
/// ```rust
/// use zeph_skills::trust::SkillSource;
///
/// let src = SkillSource::Hub { url: "https://github.com/example/skill".into() };
/// assert_eq!(src.to_string(), "hub(https://github.com/example/skill)");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SkillSource {
    /// Built-in skill shipped with the binary (bundled).
    #[default]
    Local,
    /// Downloaded from a remote URL via `skill install <url>`.
    Hub { url: String },
    /// Copied from a local directory via `skill install --path <dir>`.
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

/// Trust metadata attached to a loaded skill, stored in the trust database.
#[derive(Debug, Clone)]
pub struct SkillTrust {
    /// Skill name (matches the `name` frontmatter field).
    pub skill_name: String,
    /// Access level governing which tools the skill may invoke.
    pub trust_level: SkillTrustLevel,
    /// Provenance of the skill.
    pub source: SkillSource,
    /// blake3 hex hash of `SKILL.md` at install time, for integrity verification.
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
        // Ensure SkillTrustLevel re-exported from zeph-tools is usable
        let level: SkillTrustLevel = SkillTrustLevel::default();
        assert_eq!(level, SkillTrustLevel::Quarantined);
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
