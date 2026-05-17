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
pub use zeph_common::SkillTrustLevel;

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
// TODO: SkillSource (used in zeph-skills for user-installed provenance) intentionally has no
// `Bundled` variant — bundled skills are indistinguishable at the install API level. The trust DB
// layer (`zeph-memory::store::SourceKind`) has the `Bundled` variant and is the authoritative
// source. Align these enums if `SkillSource` ever gains first-class bundled-skill support.
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
///
/// # Tamper detection
///
/// The `blake3_hash` field records the blake3 hex digest of `SKILL.md` at install/trust-grant
/// time. When `requires_trust_check` is `true`, the agent re-hashes the file before each
/// invocation and refuses execution if the digest does not match.
///
/// **This is tamper-detection, not authentication.** A skill that was malicious when first
/// loaded and hashed will pass every per-invocation check until the hash is invalidated.
/// True trust is established by the initial user attestation step (explicit `--trust` flag or
/// `Verified` promotion in the UI) — the hash only detects post-install modifications.
#[derive(Debug, Clone)]
pub struct SkillTrust {
    /// Skill name (matches the `name` frontmatter field).
    pub skill_name: String,
    /// Access level governing which tools the skill may invoke.
    pub trust_level: SkillTrustLevel,
    /// Provenance of the skill.
    pub source: SkillSource,
    /// blake3 hex hash of `SKILL.md` at install time, for tamper detection.
    ///
    /// Used by per-invocation re-hash when [`SkillTrust::requires_trust_check`] is `true`.
    /// See the type-level doc for the security model.
    pub blake3_hash: String,
    /// Whether to re-hash `SKILL.md` on every invocation and abort if the digest changed.
    ///
    /// Set this to `true` for skills declared with `trust: high` in their frontmatter.
    /// Skills without the marker default to `false` (no per-invocation overhead).
    ///
    /// When `true`, [`compute_skill_hash`] is called before each tool dispatch. A mismatch
    /// demotes the skill to `Quarantined` and aborts the current invocation.
    pub requires_trust_check: bool,
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

    #[test]
    fn skill_trust_requires_trust_check_default_false() {
        let trust = SkillTrust {
            skill_name: "my-skill".to_owned(),
            trust_level: SkillTrustLevel::Verified,
            source: SkillSource::Local,
            blake3_hash: "abc123".to_owned(),
            requires_trust_check: false,
        };
        assert!(!trust.requires_trust_check);
    }

    #[test]
    fn skill_trust_requires_trust_check_true() {
        let trust = SkillTrust {
            skill_name: "high-trust-skill".to_owned(),
            trust_level: SkillTrustLevel::Trusted,
            source: SkillSource::Local,
            blake3_hash: "abc123".to_owned(),
            requires_trust_check: true,
        };
        assert!(trust.requires_trust_check);
    }

    #[test]
    fn hash_mismatch_detection_with_per_invocation_check() {
        let dir = tempfile::tempdir().unwrap();
        let original_content = "# My Skill\nDoes stuff.";
        std::fs::write(dir.path().join("SKILL.md"), original_content).unwrap();

        let stored_hash = compute_skill_hash(dir.path()).unwrap();
        let trust = SkillTrust {
            skill_name: "my-skill".to_owned(),
            trust_level: SkillTrustLevel::Trusted,
            source: SkillSource::Local,
            blake3_hash: stored_hash.clone(),
            requires_trust_check: true,
        };

        // Before modification: hash matches.
        let current_hash = compute_skill_hash(dir.path()).unwrap();
        assert_eq!(
            current_hash, trust.blake3_hash,
            "hash must match before modification"
        );

        // Tamper the file.
        std::fs::write(dir.path().join("SKILL.md"), "# TAMPERED\nEvil content.").unwrap();

        // After modification: hash must differ.
        let tampered_hash = compute_skill_hash(dir.path()).unwrap();
        assert_ne!(
            tampered_hash, trust.blake3_hash,
            "hash must differ after modification — tamper detected"
        );
    }
}
