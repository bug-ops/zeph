// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Skill lifecycle management: install, remove, verify, and list installed skills.
//!
//! [`SkillManager`] operates on a single `managed_dir` where user-installed skills live.
//! It does **not** touch bundled skills (those are managed by [`crate::bundled`]).
//!
//! # Install Sources
//!
//! | Method | Source |
//! |--------|--------|
//! | [`SkillManager::install_from_url`] | Shallow `git clone` from `https://` or `git@` URL |
//! | [`SkillManager::install_from_path`] | Recursive copy from a local directory |
//!
//! Both install paths validate `SKILL.md` frontmatter and compute a blake3 content hash
//! stored in the trust database by the caller.
//!
//! # Security Controls
//!
//! - URL schemes are validated before spawning `git clone` (only `https://`, `http://`, `git@`).
//! - A random temporary directory is used during clone; renamed atomically on success.
//! - [`crate::loader::validate_path_within`] is called after every filesystem operation to
//!   prevent symlink-based path traversal (REV-006).
//! - Skill names are validated to contain no path separators or `..` segments (REV-002).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::SkillError;
use crate::loader::{load_skill_meta, validate_path_within};
use crate::trust::{SkillSource, compute_skill_hash};

/// Manages the lifecycle of user-installed skills in a single managed directory.
pub struct SkillManager {
    managed_dir: PathBuf,
}

/// Result of a successful skill installation.
#[derive(Debug)]
pub struct InstallResult {
    /// Installed skill name (from the `name` frontmatter field).
    pub name: String,
    /// blake3 hex hash of the installed `SKILL.md`.
    pub blake3_hash: String,
    /// Where the skill was sourced from.
    pub source: SkillSource,
}

/// Metadata for a skill that is present in the managed directory.
#[derive(Debug)]
pub struct InstalledSkill {
    /// Skill name.
    pub name: String,
    /// Short capability description.
    pub description: String,
    /// Absolute path to the skill directory.
    pub skill_dir: PathBuf,
    /// Vault key names required at runtime.
    pub requires_secrets: Vec<String>,
}

/// Integrity verification result for an installed skill.
#[derive(Debug)]
pub struct VerifyResult {
    /// Skill name.
    pub name: String,
    /// blake3 hex hash of the current `SKILL.md` on disk.
    pub current_hash: String,
    /// `Some(true)` if the hash matches the stored value; `Some(false)` if not; `None` if no
    /// stored hash is available for comparison.
    pub stored_hash_matches: Option<bool>,
}

impl SkillManager {
    /// Create a new manager rooted at `managed_dir`.
    ///
    /// The directory is created lazily on first install.
    #[must_use]
    pub fn new(managed_dir: PathBuf) -> Self {
        Self { managed_dir }
    }

    /// Install a skill from a git URL.
    ///
    /// Clones the repository into `managed_dir/<name>`, validates SKILL.md,
    /// and computes the blake3 hash. Fails if a skill with the same name already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the URL scheme is unsupported, the clone fails,
    /// SKILL.md is invalid, or the skill already exists.
    pub fn install_from_url(&self, url: &str) -> Result<InstallResult, SkillError> {
        // Defense-in-depth: validate URL scheme inside SkillManager regardless of caller.
        if !(url.starts_with("https://") || url.starts_with("http://") || url.starts_with("git@")) {
            return Err(SkillError::GitCloneFailed(format!(
                "unsupported URL scheme: {url}"
            )));
        }
        if url.chars().any(char::is_whitespace) {
            return Err(SkillError::GitCloneFailed(
                "URL must not contain whitespace".to_owned(),
            ));
        }

        std::fs::create_dir_all(&self.managed_dir).map_err(SkillError::Io)?;

        // REV-006: combine nanos with pid to reduce predictability.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_name = format!("__tmp_{}_{}", nanos, std::process::id());
        let tmp_dir = self.managed_dir.join(&tmp_name);

        let status = Command::new("git")
            .args(["clone", "--depth=1", url, tmp_dir.to_str().unwrap_or("")])
            .status()
            .map_err(|e| SkillError::GitCloneFailed(format!("failed to run git: {e}")))?;

        if !status.success() {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(SkillError::GitCloneFailed(format!(
                "git clone failed with exit code: {}",
                status.code().unwrap_or(-1)
            )));
        }

        let skill_md = tmp_dir.join("SKILL.md");
        let meta = load_skill_meta(&skill_md).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&tmp_dir);
        })?;

        let name = meta.name.clone();
        let dest_dir = self.managed_dir.join(&name);

        if dest_dir.exists() {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(SkillError::AlreadyExists(name));
        }

        std::fs::rename(&tmp_dir, &dest_dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            SkillError::Io(e)
        })?;

        validate_path_within(&dest_dir, &self.managed_dir)?;

        strip_bundled_markers(&dest_dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&dest_dir);
            SkillError::Io(e)
        })?;

        let hash = compute_skill_hash(&dest_dir)?;

        Ok(InstallResult {
            name,
            blake3_hash: hash,
            source: SkillSource::Hub {
                url: url.to_owned(),
            },
        })
    }

    /// Install a skill from a local directory path.
    ///
    /// Copies the directory into `managed_dir/<name>`, validates SKILL.md,
    /// and computes the blake3 hash.
    ///
    /// # Errors
    ///
    /// Returns an error if copy fails, SKILL.md is invalid, or the skill already exists.
    pub fn install_from_path(&self, source: &Path) -> Result<InstallResult, SkillError> {
        std::fs::create_dir_all(&self.managed_dir).map_err(SkillError::Io)?;

        let skill_md = source.join("SKILL.md");
        let meta = load_skill_meta(&skill_md)?;
        let name = meta.name.clone();

        // REV-002: validate the name contains no path separators or ".." before any writes.
        // load_skill_meta already enforces lowercase+hyphen only names, so this is
        // an additional defense-in-depth check.
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(SkillError::Invalid(format!("invalid skill name: {name}")));
        }

        let dest_dir = self.managed_dir.join(&name);
        if dest_dir.exists() {
            return Err(SkillError::AlreadyExists(name));
        }

        copy_dir_recursive(source, &dest_dir).map_err(|e| {
            SkillError::CopyFailed(format!("failed to copy {}: {e}", source.display()))
        })?;

        // Secondary check after copy to catch symlink-based escapes.
        validate_path_within(&dest_dir, &self.managed_dir)?;

        strip_bundled_markers(&dest_dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&dest_dir);
            SkillError::Io(e)
        })?;

        let hash = compute_skill_hash(&dest_dir)?;

        Ok(InstallResult {
            name: name.clone(),
            blake3_hash: hash,
            source: SkillSource::File {
                path: source.to_owned(),
            },
        })
    }

    /// Remove an installed skill directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the skill is not found or removal fails.
    pub fn remove(&self, name: &str) -> Result<(), SkillError> {
        let skill_dir = self.managed_dir.join(name);
        if !skill_dir.exists() {
            return Err(SkillError::NotFound(name.to_owned()));
        }
        validate_path_within(&skill_dir, &self.managed_dir)?;
        std::fs::remove_dir_all(&skill_dir).map_err(SkillError::Io)?;
        Ok(())
    }

    /// List all installed skills with filesystem metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the managed directory cannot be read.
    pub fn list_installed(&self) -> Result<Vec<InstalledSkill>, SkillError> {
        if !self.managed_dir.exists() {
            return Ok(Vec::new());
        }

        // REV-005: canonicalize managed_dir once outside the loop.
        let canonical_base = self.managed_dir.canonicalize().map_err(|e| {
            SkillError::Other(format!(
                "failed to canonicalize managed dir {}: {e}",
                self.managed_dir.display()
            ))
        })?;

        let mut result = Vec::new();
        let entries = std::fs::read_dir(&self.managed_dir).map_err(SkillError::Io)?;

        for entry in entries.flatten() {
            let skill_dir = entry.path();
            let skill_md = skill_dir.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            if validate_path_within(&skill_md, &canonical_base).is_err() {
                continue;
            }
            match load_skill_meta(&skill_md) {
                Ok(meta) => result.push(InstalledSkill {
                    name: meta.name,
                    description: meta.description,
                    skill_dir,
                    requires_secrets: meta.requires_secrets,
                }),
                Err(e) => tracing::warn!("skipping {}: {e:#}", skill_md.display()),
            }
        }

        Ok(result)
    }

    /// Recompute the blake3 hash for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the skill directory is not found or hashing fails.
    pub fn verify(&self, name: &str) -> Result<String, SkillError> {
        let skill_dir = self.managed_dir.join(name);
        if !skill_dir.exists() {
            return Err(SkillError::NotFound(name.to_owned()));
        }
        validate_path_within(&skill_dir, &self.managed_dir)?;
        compute_skill_hash(&skill_dir).map_err(SkillError::Io)
    }

    /// Verify all installed skills and compare with stored hashes.
    ///
    /// `stored_hashes` maps skill name to the hash stored in the database.
    ///
    /// # Errors
    ///
    /// Returns an error if listing installed skills fails.
    pub fn verify_all(
        &self,
        stored_hashes: &std::collections::HashMap<String, String>,
    ) -> Result<Vec<VerifyResult>, SkillError> {
        let installed = self.list_installed()?;
        let mut results = Vec::new();

        for skill in installed {
            match compute_skill_hash(&skill.skill_dir) {
                Ok(current_hash) => {
                    let stored_hash_matches = stored_hashes
                        .get(&skill.name)
                        .map(|stored| stored == &current_hash);
                    results.push(VerifyResult {
                        name: skill.name,
                        current_hash,
                        stored_hash_matches,
                    });
                }
                Err(e) => {
                    tracing::warn!("failed to hash skill '{}': {e:#}", skill.name);
                }
            }
        }

        Ok(results)
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        // M2: skip symlinks — symlink targets may point outside the skill tree.
        // validate_path_within after copy catches escapes, but skipping is cleaner.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            tracing::warn!(
                path = %src_path.display(),
                "skipping symlink in skill source directory"
            );
            continue;
        }
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Remove `.bundled` marker files from `dir` recursively.
///
/// Hub-installed packages must not contain `.bundled` markers because
/// their presence bypasses the content security scanner in [`crate::registry`] (see
/// [#3040](https://github.com/example/zeph/issues/3040)).
///
/// Each removal is logged at `WARN` level as a security event. Returns the
/// number of markers removed.
///
/// # Errors
///
/// Returns an error if any removal fails (e.g. permission denied).
fn strip_bundled_markers(dir: &Path) -> std::io::Result<u64> {
    strip_bundled_markers_recursive(dir)
}

fn strip_bundled_markers_recursive(dir: &Path) -> std::io::Result<u64> {
    let mut removed = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            removed += strip_bundled_markers_recursive(&path)?;
        } else if file_type.is_file() && entry.file_name() == ".bundled" {
            tracing::warn!(
                path = %path.display(),
                "stripped forged .bundled marker from installed skill package"
            );
            std::fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_skill_dir(dir: &Path, name: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: A test skill.\n---\n# Body\nHello"),
        )
        .unwrap();
    }

    #[test]
    fn install_from_url_rejects_bad_scheme() {
        let managed = tempfile::tempdir().unwrap();
        let mgr = SkillManager::new(managed.path().to_path_buf());
        let err = mgr.install_from_url("ftp://example.com/skill").unwrap_err();
        assert!(matches!(err, SkillError::GitCloneFailed(_)));
        assert!(format!("{err}").contains("unsupported URL scheme"));
    }

    #[test]
    fn install_from_url_rejects_whitespace() {
        let managed = tempfile::tempdir().unwrap();
        let mgr = SkillManager::new(managed.path().to_path_buf());
        let err = mgr
            .install_from_url("https://example.com/skill name")
            .unwrap_err();
        assert!(matches!(err, SkillError::GitCloneFailed(_)));
        assert!(format!("{err}").contains("whitespace"));
    }

    #[test]
    fn install_from_path_success() {
        let src = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        make_skill_dir(src.path(), "my-skill");

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let result = mgr.install_from_path(&src.path().join("my-skill")).unwrap();

        assert_eq!(result.name, "my-skill");
        assert_eq!(result.blake3_hash.len(), 64);
        assert!(matches!(result.source, SkillSource::File { .. }));
        assert!(managed.path().join("my-skill").join("SKILL.md").exists());
    }

    #[test]
    fn install_from_path_already_exists() {
        let src = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        make_skill_dir(src.path(), "dup-skill");
        make_skill_dir(managed.path(), "dup-skill");

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let err = mgr
            .install_from_path(&src.path().join("dup-skill"))
            .unwrap_err();
        assert!(matches!(err, SkillError::AlreadyExists(_)));
    }

    #[test]
    fn install_from_path_invalid_skill() {
        let src = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let bad_dir = src.path().join("bad-skill");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("SKILL.md"), "no frontmatter").unwrap();

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let err = mgr.install_from_path(&bad_dir).unwrap_err();
        assert!(
            format!("{err}").contains("missing frontmatter")
                || format!("{err}").contains("invalid")
        );
    }

    #[test]
    fn remove_skill_success() {
        let managed = tempfile::tempdir().unwrap();
        make_skill_dir(managed.path(), "to-remove");

        let mgr = SkillManager::new(managed.path().to_path_buf());
        mgr.remove("to-remove").unwrap();
        assert!(!managed.path().join("to-remove").exists());
    }

    #[test]
    fn remove_skill_not_found() {
        let managed = tempfile::tempdir().unwrap();
        let mgr = SkillManager::new(managed.path().to_path_buf());
        let err = mgr.remove("nonexistent").unwrap_err();
        assert!(matches!(err, SkillError::NotFound(_)));
    }

    #[test]
    fn list_installed_empty_dir() {
        let managed = tempfile::tempdir().unwrap();
        let mgr = SkillManager::new(managed.path().to_path_buf());
        let list = mgr.list_installed().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn list_installed_nonexistent_dir() {
        let mgr = SkillManager::new(PathBuf::from("/nonexistent/managed/dir"));
        let list = mgr.list_installed().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn list_installed_with_skills() {
        let managed = tempfile::tempdir().unwrap();
        make_skill_dir(managed.path(), "skill-a");
        make_skill_dir(managed.path(), "skill-b");

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let mut list = mgr.list_installed().unwrap();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "skill-a");
        assert_eq!(list[1].name, "skill-b");
    }

    #[test]
    fn verify_skill_success() {
        let managed = tempfile::tempdir().unwrap();
        make_skill_dir(managed.path(), "verify-me");

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let hash = mgr.verify("verify-me").unwrap();
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn verify_skill_not_found() {
        let managed = tempfile::tempdir().unwrap();
        let mgr = SkillManager::new(managed.path().to_path_buf());
        let err = mgr.verify("nope").unwrap_err();
        assert!(matches!(err, SkillError::NotFound(_)));
    }

    #[test]
    fn verify_all_with_matching_hash() {
        let managed = tempfile::tempdir().unwrap();
        make_skill_dir(managed.path(), "hash-skill");

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let hash = mgr.verify("hash-skill").unwrap();

        let mut stored = std::collections::HashMap::new();
        stored.insert("hash-skill".to_owned(), hash);

        let results = mgr.verify_all(&stored).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].stored_hash_matches, Some(true));
    }

    #[test]
    fn verify_all_with_mismatched_hash() {
        let managed = tempfile::tempdir().unwrap();
        make_skill_dir(managed.path(), "tampered-skill");

        let mgr = SkillManager::new(managed.path().to_path_buf());

        let mut stored = std::collections::HashMap::new();
        stored.insert("tampered-skill".to_owned(), "wrong_hash".to_owned());

        let results = mgr.verify_all(&stored).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].stored_hash_matches, Some(false));
    }

    #[test]
    fn verify_all_no_stored_hash() {
        let managed = tempfile::tempdir().unwrap();
        make_skill_dir(managed.path(), "unknown-skill");

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let results = mgr.verify_all(&std::collections::HashMap::new()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].stored_hash_matches, None);
    }

    #[test]
    fn install_from_url_accepts_git_at_scheme() {
        let managed = tempfile::tempdir().unwrap();
        let mgr = SkillManager::new(managed.path().to_path_buf());
        // git@ is accepted by URL validation; git clone will fail (no network),
        // but the error should be GitCloneFailed — not "unsupported URL scheme".
        let err = mgr
            .install_from_url("git@github.com:example/skill.git")
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            !msg.contains("unsupported URL scheme"),
            "git@ scheme should pass URL check: {msg}"
        );
        assert!(matches!(err, SkillError::GitCloneFailed(_)));
    }

    #[test]
    fn install_from_url_rejects_empty_string() {
        let managed = tempfile::tempdir().unwrap();
        let mgr = SkillManager::new(managed.path().to_path_buf());
        let err = mgr.install_from_url("").unwrap_err();
        assert!(matches!(err, SkillError::GitCloneFailed(_)));
        assert!(format!("{err}").contains("unsupported URL scheme"));
    }

    #[test]
    fn install_from_path_missing_source_dir() {
        let managed = tempfile::tempdir().unwrap();
        let mgr = SkillManager::new(managed.path().to_path_buf());
        let err = mgr
            .install_from_path(Path::new("/nonexistent/skill/path"))
            .unwrap_err();
        // load_skill_meta reads SKILL.md → file not found
        let msg = format!("{err}");
        assert!(
            msg.contains("No such file")
                || msg.contains("cannot find")
                || msg.contains("invalid")
                || msg.contains("missing"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn install_from_path_missing_skill_md() {
        let src = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        // Create source dir but no SKILL.md inside it
        std::fs::create_dir_all(src.path().join("skill-no-md")).unwrap();

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let err = mgr
            .install_from_path(&src.path().join("skill-no-md"))
            .unwrap_err();
        // load_skill_meta opens SKILL.md → file not found
        let msg = format!("{err}");
        assert!(
            msg.contains("No such file")
                || msg.contains("cannot find")
                || msg.contains("invalid")
                || msg.contains("missing"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn list_installed_skips_dirs_without_skill_md() {
        let managed = tempfile::tempdir().unwrap();
        // Create a real skill dir with SKILL.md
        make_skill_dir(managed.path(), "valid-skill");
        // Create a dir without SKILL.md — should be skipped
        std::fs::create_dir_all(managed.path().join("no-md-dir")).unwrap();

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let list = mgr.list_installed().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "valid-skill");
    }

    #[test]
    fn verify_all_empty_dir_returns_empty() {
        let managed = tempfile::tempdir().unwrap();
        let mgr = SkillManager::new(managed.path().to_path_buf());
        let results = mgr.verify_all(&std::collections::HashMap::new()).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn verify_all_multiple_skills() {
        let managed = tempfile::tempdir().unwrap();
        make_skill_dir(managed.path(), "skill-one");
        make_skill_dir(managed.path(), "skill-two");

        let mgr = SkillManager::new(managed.path().to_path_buf());

        let hash_one = mgr.verify("skill-one").unwrap();
        let mut stored = std::collections::HashMap::new();
        stored.insert("skill-one".to_owned(), hash_one);
        stored.insert("skill-two".to_owned(), "stale-hash".to_owned());

        let mut results = mgr.verify_all(&stored).unwrap();
        results.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].stored_hash_matches, Some(true));
        assert_eq!(results[1].stored_hash_matches, Some(false));
    }

    #[test]
    fn remove_skill_path_traversal_rejected() {
        let managed = tempfile::tempdir().unwrap();
        let mgr = SkillManager::new(managed.path().to_path_buf());
        // "../something" should either be NotFound or PathTraversal
        let err = mgr.remove("../evil").unwrap_err();
        // The dir won't exist so we expect NotFound or PathTraversal
        assert!(
            matches!(
                err,
                SkillError::NotFound(_) | SkillError::Invalid(_) | SkillError::Other(_)
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn install_from_url_rejects_tab_in_url() {
        let managed = tempfile::tempdir().unwrap();
        let mgr = SkillManager::new(managed.path().to_path_buf());
        let err = mgr
            .install_from_url("https://example.com/skill\ttab")
            .unwrap_err();
        assert!(matches!(err, SkillError::GitCloneFailed(_)));
        assert!(format!("{err}").contains("whitespace"));
    }

    #[test]
    fn list_installed_populates_requires_secrets() {
        let managed = tempfile::tempdir().unwrap();
        let skill_dir = managed.path().join("api-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: api-skill\ndescription: Needs secrets.\nx-requires-secrets: github_token, slack_webhook\n---\n# Body\nHello",
        )
        .unwrap();

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let list = mgr.list_installed().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "api-skill");
        assert_eq!(
            list[0].requires_secrets,
            vec!["github_token".to_owned(), "slack_webhook".to_owned()]
        );
    }

    #[test]
    fn new_manager_stores_path() {
        let dir = PathBuf::from("/some/path");
        let mgr = SkillManager::new(dir.clone());
        // verify basic construction — managed_dir is private, but list_installed
        // on nonexistent path returns Ok([])
        let result = mgr.list_installed();
        assert!(result.is_ok());
    }

    // --- security: .bundled marker stripping ---

    #[test]
    fn install_from_path_strips_bundled_marker() {
        let src = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();

        let skill_src = src.path().join("sec-skill");
        std::fs::create_dir_all(&skill_src).unwrap();
        std::fs::write(
            skill_src.join("SKILL.md"),
            "---\nname: sec-skill\ndescription: Security test.\n---\n# Body\nHello",
        )
        .unwrap();
        // Forge the .bundled marker
        std::fs::write(skill_src.join(".bundled"), "0.1.0").unwrap();

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let result = mgr.install_from_path(&skill_src).unwrap();

        assert_eq!(result.name, "sec-skill");
        let installed = managed.path().join("sec-skill");
        assert!(
            installed.join("SKILL.md").exists(),
            "SKILL.md must be present"
        );
        assert!(
            !installed.join(".bundled").exists(),
            ".bundled must be stripped after install"
        );
    }

    #[test]
    fn install_from_path_strips_nested_bundled_marker() {
        let src = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();

        let skill_src = src.path().join("nested-skill");
        let subdir = skill_src.join("scripts");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(
            skill_src.join("SKILL.md"),
            "---\nname: nested-skill\ndescription: Nested test.\n---\n# Body\nHello",
        )
        .unwrap();
        // Forge .bundled at root and in a subdirectory
        std::fs::write(skill_src.join(".bundled"), "0.1.0").unwrap();
        std::fs::write(subdir.join(".bundled"), "0.1.0").unwrap();

        let mgr = SkillManager::new(managed.path().to_path_buf());
        mgr.install_from_path(&skill_src).unwrap();

        let installed = managed.path().join("nested-skill");
        assert!(
            !installed.join(".bundled").exists(),
            "root .bundled must be stripped"
        );
        assert!(
            !installed.join("scripts").join(".bundled").exists(),
            "nested .bundled must be stripped"
        );
    }

    #[test]
    fn install_from_path_forged_bundled_stays_quarantined() {
        // Trust level is determined by managed_dir membership, not .bundled.
        // After stripping, the result must still be SkillSource::File (path install).
        // The actual Quarantined trust assignment happens in the caller (agent), but
        // we verify install returns SkillSource::File and .bundled is gone.
        let src = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();

        let skill_src = src.path().join("q-skill");
        std::fs::create_dir_all(&skill_src).unwrap();
        std::fs::write(
            skill_src.join("SKILL.md"),
            "---\nname: q-skill\ndescription: Quarantine test.\n---\n# Body\nHello",
        )
        .unwrap();
        std::fs::write(skill_src.join(".bundled"), "forged").unwrap();

        let mgr = SkillManager::new(managed.path().to_path_buf());
        let result = mgr.install_from_path(&skill_src).unwrap();

        assert!(
            matches!(result.source, SkillSource::File { .. }),
            "source must be File for path install"
        );
        assert!(
            !managed.path().join("q-skill").join(".bundled").exists(),
            ".bundled must not exist after install"
        );
    }

    #[test]
    fn strip_bundled_markers_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let count = strip_bundled_markers(dir.path()).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn strip_bundled_markers_preserves_other_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "content").unwrap();
        std::fs::write(dir.path().join("script.sh"), "#!/bin/sh").unwrap();
        std::fs::write(dir.path().join(".bundled"), "0.1.0").unwrap();

        strip_bundled_markers(dir.path()).unwrap();

        assert!(
            dir.path().join("SKILL.md").exists(),
            "SKILL.md must be preserved"
        );
        assert!(
            dir.path().join("script.sh").exists(),
            "script.sh must be preserved"
        );
        assert!(
            !dir.path().join(".bundled").exists(),
            ".bundled must be removed"
        );
    }

    #[test]
    fn strip_bundled_markers_removes_at_multiple_levels() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join(".bundled"), "0.1.0").unwrap();
        std::fs::write(sub.join(".bundled"), "0.1.0").unwrap();
        std::fs::write(sub.join("keep.txt"), "data").unwrap();

        let count = strip_bundled_markers(dir.path()).unwrap();

        assert_eq!(count, 2, "both .bundled files must be removed");
        assert!(!dir.path().join(".bundled").exists());
        assert!(!sub.join(".bundled").exists());
        assert!(sub.join("keep.txt").exists(), "keep.txt must survive");
    }
}
