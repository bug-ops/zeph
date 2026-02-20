use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::SkillError;
use crate::loader::{load_skill_meta, validate_path_within};
use crate::trust::{SkillSource, compute_skill_hash};

pub struct SkillManager {
    managed_dir: PathBuf,
}

#[derive(Debug)]
pub struct InstallResult {
    pub name: String,
    pub blake3_hash: String,
    pub source: SkillSource,
}

#[derive(Debug)]
pub struct InstalledSkill {
    pub name: String,
    pub description: String,
    pub skill_dir: PathBuf,
}

#[derive(Debug)]
pub struct VerifyResult {
    pub name: String,
    pub current_hash: String,
    pub stored_hash_matches: Option<bool>,
}

impl SkillManager {
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
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
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
    fn new_manager_stores_path() {
        let dir = PathBuf::from("/some/path");
        let mgr = SkillManager::new(dir.clone());
        // verify basic construction — managed_dir is private, but list_installed
        // on nonexistent path returns Ok([])
        let result = mgr.list_installed();
        assert!(result.is_ok());
    }
}
