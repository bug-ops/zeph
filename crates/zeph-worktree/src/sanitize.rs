// SPDX-License-Identifier: MIT
//! Input sanitisation for branch components and worktree root paths.
//!
//! Every value that ends up in a `git` invocation passes through one of the
//! functions here before being used.  This module is the enforcement point for
//! the `NEVER` invariants in the spec:
//! - branch components are validated before use
//! - root paths are canonicalised and confined to the repository tree

use std::path::{Path, PathBuf};

use crate::error::WorktreeError;

/// Validates that `s` is safe to use as a git branch component.
///
/// A valid component matches `^[A-Za-z0-9._-]+$`, must not start with `-` or
/// `.`, and must not contain the substrings `..` or `/`.
///
/// # Errors
///
/// Returns [`WorktreeError::InvalidBranchName`] when any rule is violated.
///
/// # Examples
///
/// ```no_run
/// use zeph_worktree::sanitize::validate_branch_component;
///
/// validate_branch_component("agent-42").unwrap();
/// assert!(validate_branch_component("../escape").is_err());
/// assert!(validate_branch_component("-leading-dash").is_err());
/// ```
pub fn validate_branch_component(s: &str) -> Result<(), WorktreeError> {
    // Must not be empty, and must not start with '-' or '.'
    match s.chars().next() {
        None | Some('-' | '.') => {
            return Err(WorktreeError::InvalidBranchName(s.to_string()));
        }
        Some(_) => {}
    }

    // Must not contain '..' or '/'
    if s.contains("..") || s.contains('/') {
        return Err(WorktreeError::InvalidBranchName(s.to_string()));
    }

    // All characters must be in [A-Za-z0-9._-]
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(WorktreeError::InvalidBranchName(s.to_string()));
    }

    Ok(())
}

/// Canonicalises `root` relative to `repo_root`, rejecting paths that escape
/// the repository.
///
/// If `root` is relative, it is joined onto `repo_root` before canonicalisation.
/// The resulting canonical path must have `repo_root`'s canonical path as a
/// prefix; paths that resolve outside emit [`WorktreeError::RootOutsideRepo`].
///
/// The directory is created with [`std::fs::create_dir_all`] before
/// canonicalisation so that `canonicalize` does not fail on a not-yet-existing
/// worktree root.
///
/// # Errors
///
/// - [`WorktreeError::RootOutsideRepo`] if the path escapes the repository.
/// - [`WorktreeError::Io`] for any underlying I/O failure.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use zeph_worktree::sanitize::canonicalize_root;
///
/// let repo = Path::new("/tmp/myrepo");
/// let root = canonicalize_root(Path::new(".claude/worktrees"), repo).unwrap();
/// assert!(root.starts_with(repo));
/// ```
pub fn canonicalize_root(root: &Path, repo_root: &Path) -> Result<PathBuf, WorktreeError> {
    let candidate = if root.is_relative() {
        repo_root.join(root)
    } else {
        root.to_path_buf()
    };

    // Create the directory so canonicalize doesn't fail for non-existent paths.
    std::fs::create_dir_all(&candidate)?;

    let canonical_root = std::fs::canonicalize(&candidate)?;
    let canonical_repo = std::fs::canonicalize(repo_root)?;

    if !canonical_root.starts_with(&canonical_repo) {
        return Err(WorktreeError::RootOutsideRepo(canonical_root));
    }

    Ok(canonical_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- validate_branch_component ---

    #[test]
    fn valid_simple() {
        assert!(validate_branch_component("agent-42").is_ok());
        assert!(validate_branch_component("feat.work").is_ok());
        assert!(validate_branch_component("A_B_C").is_ok());
        assert!(validate_branch_component("abc123").is_ok());
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_branch_component("").is_err());
    }

    #[test]
    fn rejects_leading_dash() {
        assert!(validate_branch_component("-bad").is_err());
    }

    #[test]
    fn rejects_leading_dot() {
        assert!(validate_branch_component(".git").is_err());
    }

    #[test]
    fn rejects_double_dot() {
        assert!(validate_branch_component("a..b").is_err());
        assert!(validate_branch_component("../escape").is_err());
    }

    #[test]
    fn rejects_slash() {
        assert!(validate_branch_component("a/b").is_err());
    }

    #[test]
    fn rejects_special_chars() {
        assert!(validate_branch_component("ag@nt").is_err());
        assert!(validate_branch_component("ag nt").is_err());
        assert!(validate_branch_component("ag:nt").is_err());
    }

    // --- canonicalize_root ---

    #[test]
    fn root_inside_repo_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let canonical_repo = std::fs::canonicalize(repo).unwrap();
        let result = canonicalize_root(std::path::Path::new("worktrees"), repo);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        assert!(result.unwrap().starts_with(&canonical_repo));
    }

    #[test]
    fn root_outside_repo_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("inner");
        std::fs::create_dir_all(&repo).unwrap();
        // Absolute path pointing to the parent of repo_root escapes confinement.
        let parent = dir.path().to_path_buf();
        let err = canonicalize_root(&parent, &repo).unwrap_err();
        assert!(matches!(err, WorktreeError::RootOutsideRepo(_)));
    }

    #[test]
    fn absolute_root_inside_repo_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let result = canonicalize_root(&sub, repo);
        assert!(result.is_ok());
    }
}
