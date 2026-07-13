// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared sandbox-boundary check for `allowed_paths`-style path validation.
//!
//! Extracted from the near-identical `validate_path` bodies in
//! `zeph-tools::file::FileExecutor` and `zeph-tools::diagnostics::DiagnosticsExecutor` (#6032
//! SEC-2) so every caller enforcing an `allowed_paths` sandbox — including
//! `zeph-tools::cwd::resolve_and_set_cwd`, the third caller this module was extracted for —
//! shares one canonical `starts_with`-against-allowed-roots check instead of three
//! textually-similar-but-independently-maintained copies.
//!
//! Deliberately does **not** own path *resolution* strategy (tilde-expansion, relative-path
//! joining, or symlink-tolerant canonicalization of a not-yet-existing target): callers differ
//! legitimately there — `FileExecutor` must tolerate a nonexistent target (writing a new
//! file), while `DiagnosticsExecutor` and `resolve_and_set_cwd` require the target to already
//! exist. Only the final containment check — the actual security invariant — is shared.

use std::io;
use std::path::{Path, PathBuf};

/// Returns `true` if `canonical` is contained within (or equal to) at least one of
/// `allowed_paths`.
///
/// `canonical` must already be canonicalized (symlinks resolved) by the caller — this
/// function performs no filesystem access itself, so it is safe to call on a path that does
/// not fully exist yet (e.g. `FileExecutor`'s ancestor-resolved-but-not-yet-created target).
#[must_use]
pub fn is_path_within(canonical: &Path, allowed_paths: &[PathBuf]) -> bool {
    allowed_paths.iter().any(|a| canonical.starts_with(a))
}

/// Canonicalize `path` (which must already exist) and verify it falls within one of
/// `allowed_paths`.
///
/// Convenience wrapper around [`is_path_within`] for the common case of a target that must
/// already exist (e.g. a directory to `cd` into, or a file to run diagnostics against) —
/// callers whose target may not yet exist (e.g. a new file being written) must canonicalize
/// via their own symlink-tolerant strategy and call [`is_path_within`] directly instead.
///
/// # Errors
///
/// Returns [`io::ErrorKind::NotFound`] (via [`Path::canonicalize`]) if `path` does not exist,
/// or [`io::ErrorKind::PermissionDenied`] if the canonicalized path falls outside every entry
/// in `allowed_paths`.
///
/// # Examples
///
/// ```
/// use zeph_common::security::validate_path_within;
///
/// let dir = tempfile::tempdir().unwrap();
/// // Callers canonicalize `allowed_paths` up front (as `FileExecutor::new` does) so this
/// // comparison is not defeated by a symlinked temp root (e.g. macOS `/tmp` -> `/private/tmp`).
/// let allowed = vec![dir.path().canonicalize().unwrap()];
/// let result = validate_path_within(dir.path(), &allowed);
/// assert!(result.is_ok());
/// ```
pub fn validate_path_within(path: &Path, allowed_paths: &[PathBuf]) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    if !is_path_within(&canonical, allowed_paths) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "path '{}' is outside the allowed sandbox",
                canonical.display()
            ),
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_path_within_true_for_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];
        assert!(is_path_within(dir.path(), &allowed));
    }

    #[test]
    fn is_path_within_true_for_nested_path() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];
        let nested = dir.path().join("a").join("b");
        assert!(is_path_within(&nested, &allowed));
    }

    #[test]
    fn is_path_within_false_for_sibling_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let sibling = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().to_path_buf()];
        assert!(!is_path_within(sibling.path(), &allowed));
    }

    #[test]
    fn is_path_within_true_when_any_of_multiple_roots_matches() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let allowed = vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()];
        assert!(is_path_within(dir_b.path(), &allowed));
    }

    #[test]
    fn validate_path_within_ok_for_existing_path_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize the allowed root up front — mirrors `FileExecutor::new`/
        // `DiagnosticsExecutor::new`'s real construction pattern, and avoids a false
        // rejection on platforms where the tempdir root is itself a symlink (e.g. macOS
        // `/tmp` -> `/private/tmp`).
        let allowed = vec![dir.path().canonicalize().unwrap()];
        let result = validate_path_within(dir.path(), &allowed);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_path_within_rejects_path_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().canonicalize().unwrap()];
        let result = validate_path_within(outside.path(), &allowed);
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn validate_path_within_errors_on_nonexistent_path() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().canonicalize().unwrap()];
        let missing = dir.path().join("does-not-exist");
        let result = validate_path_within(&missing, &allowed);
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
