// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(dead_code)]
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors returned by [`validate_deep_link_cwd`].
#[derive(Debug, Error)]
pub enum CwdValidationError {
    /// The supplied path is not absolute.
    #[error("path must be absolute")]
    NotAbsolute,
    /// `std::fs::canonicalize` failed (I/O error or path does not exist).
    #[error("canonicalization failed: {0}")]
    CanonicalizeFailed(std::io::Error),
    /// The canonical path matches a hardcoded denylist entry.
    #[error("path is in a denied location: {0}")]
    Denied(String),
    /// `allowed_roots` is non-empty and the canonical path does not start with any root.
    #[error("path is outside allowed roots")]
    OutsideAllowedRoots,
    /// The canonical path exists but is not a directory.
    #[error("path is not a directory")]
    NotADirectory,
}

impl From<std::io::Error> for CwdValidationError {
    fn from(e: std::io::Error) -> Self {
        Self::CanonicalizeFailed(e)
    }
}

/// Validates a cwd path received from a deep-link URI for safe use as a working directory.
///
/// Follows INV-CWD from spec §3: absolute → canonicalize → case-fold →
/// denylist → allowlist → `is_dir`. Steps must not be reordered.
///
/// # Errors
///
/// Returns [`CwdValidationError`] on the first validation failure.
///
/// # Examples
///
/// ```no_run
/// use std::path::PathBuf;
/// # use zeph::url_scheme::validate::validate_deep_link_cwd;
/// let result = validate_deep_link_cwd(&PathBuf::from("/home/user/project"), &[]);
/// assert!(result.is_ok());
/// ```
pub fn validate_deep_link_cwd(
    raw: &Path,
    allowed_roots: &[PathBuf],
) -> Result<PathBuf, CwdValidationError> {
    // Step 1: assert path is absolute.
    if !raw.is_absolute() {
        return Err(CwdValidationError::NotAbsolute);
    }

    // Step 2: canonicalize to resolve symlinks, .., and ..
    // SAFETY: TOCTOU window between canonicalize and use is accepted per spec §3.
    let canonical = std::fs::canonicalize(raw)?;

    // Step 3: case-fold for comparison (lowercase on macOS/Windows; no-op on Linux).
    let canonical_str = canonical.to_string_lossy().into_owned();
    #[cfg(not(target_os = "linux"))]
    let folded = canonical_str.to_lowercase();
    #[cfg(target_os = "linux")]
    let folded = canonical_str.clone();

    // Step 4: compare against hardcoded denylist (case-folded root prefixes).
    let denied = build_denylist();
    for entry in &denied {
        // A denylist entry "/proc" must block "/proc" and "/proc/anything" but not "/processing".
        // Use starts_with + boundary check: either exact match or followed by '/'.
        let is_exact = folded == *entry;
        let is_prefix = folded.starts_with(&format!("{entry}/"));
        if is_exact || is_prefix {
            return Err(CwdValidationError::Denied(canonical_str));
        }
    }

    // Step 5: if allowed_roots non-empty, assert starts_with at least one root.
    if !allowed_roots.is_empty() {
        let allowed = allowed_roots.iter().any(|root| canonical.starts_with(root));
        if !allowed {
            return Err(CwdValidationError::OutsideAllowedRoots);
        }
    }

    // Step 6: assert metadata().is_dir().
    let meta = canonical.metadata()?;
    if !meta.is_dir() {
        return Err(CwdValidationError::NotADirectory);
    }

    Ok(canonical)
}

/// Builds the case-folded hardcoded denylist of root prefixes.
///
/// On non-Linux platforms the entries are lowercased; on Linux they are returned as-is
/// since filesystem paths are case-sensitive.
///
/// # Note
///
/// When `HOME` is unset or empty, home-directory entries are omitted from the denylist.
/// This is safe: an unset `HOME` typically indicates a sandboxed or service environment
/// where the user's home directory is not accessible.
pub(crate) fn build_denylist() -> Vec<String> {
    #[cfg(target_os = "linux")]
    let static_entries: &[&str] = &["/proc", "/sys", "/dev", "/etc", "/root", "/boot", "/run"];
    #[cfg(target_os = "macos")]
    let static_entries: &[&str] = &["/proc", "/sys", "/dev", "/System", "/Library/Keychains"];
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let static_entries: &[&str] = &["/proc", "/sys", "/dev"];

    let home = std::env::var("HOME").unwrap_or_default();

    let home_entries: Vec<String> = if home.is_empty() {
        Vec::new()
    } else {
        vec![
            format!("{home}/.ssh"),
            format!("{home}/.gnupg"),
            format!("{home}/.aws"),
        ]
    };

    let mut result: Vec<String> = static_entries
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    result.extend(home_entries);

    #[cfg(target_os = "windows")]
    {
        use std::env;
        if let Ok(sysroot) = env::var("SystemRoot") {
            result.push(sysroot);
        }
        if let Ok(windir) = env::var("WINDIR") {
            result.push(windir);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        result = result.into_iter().map(|s| s.to_lowercase()).collect();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn absolute_path_to_temp_dir_is_ok() {
        let dir = std::env::temp_dir();
        let result = validate_deep_link_cwd(&dir, &[]);
        assert!(result.is_ok(), "temp dir should be valid: {result:?}");
    }

    #[test]
    fn relative_path_returns_not_absolute() {
        let result = validate_deep_link_cwd(Path::new("relative/path"), &[]);
        assert!(
            matches!(result, Err(CwdValidationError::NotAbsolute)),
            "expected NotAbsolute, got {result:?}"
        );
    }

    #[test]
    fn nonexistent_path_returns_canonicalize_failed() {
        let result = validate_deep_link_cwd(
            Path::new("/this/path/definitely/does/not/exist/zeph_test"),
            &[],
        );
        assert!(
            matches!(result, Err(CwdValidationError::CanonicalizeFailed(_))),
            "expected CanonicalizeFailed, got {result:?}"
        );
    }

    #[test]
    fn file_path_returns_not_a_directory() {
        let tmp = std::env::temp_dir();
        let file_path = tmp.join("zeph_cwd_test_file.txt");
        fs::write(&file_path, b"test").expect("write test file");
        let result = validate_deep_link_cwd(&file_path, &[]);
        let _ = fs::remove_file(&file_path);
        assert!(
            matches!(result, Err(CwdValidationError::NotADirectory)),
            "expected NotADirectory, got {result:?}"
        );
    }

    #[test]
    fn path_outside_allowed_roots_returns_error() {
        let dir = std::env::temp_dir();
        let allowed = vec![PathBuf::from("/nonexistent/root/zeph_allowed")];
        let result = validate_deep_link_cwd(&dir, &allowed);
        assert!(
            matches!(result, Err(CwdValidationError::OutsideAllowedRoots)),
            "expected OutsideAllowedRoots, got {result:?}"
        );
    }

    #[test]
    fn path_within_allowed_roots_is_ok() {
        let dir = std::env::temp_dir();
        // Canonicalize the allowed root so it matches what validate_deep_link_cwd produces.
        // On macOS /var/folders resolves to /private/var/folders after canonicalize.
        let canonical_root = std::fs::canonicalize(&dir).expect("canonicalize temp dir");
        let allowed = vec![canonical_root];
        let result = validate_deep_link_cwd(&dir, &allowed);
        assert!(
            result.is_ok(),
            "path within allowed roots should pass: {result:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_path_is_denied() {
        let result = validate_deep_link_cwd(Path::new("/proc"), &[]);
        assert!(
            matches!(result, Err(CwdValidationError::Denied(_))),
            "expected Denied for /proc, got {result:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sys_path_is_denied() {
        let result = validate_deep_link_cwd(Path::new("/sys"), &[]);
        assert!(
            matches!(result, Err(CwdValidationError::Denied(_))),
            "expected Denied for /sys, got {result:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn home_dirs_denied_macos() {
        if let Ok(home) = std::env::var("HOME") {
            let ssh_path = std::path::PathBuf::from(&home).join(".ssh");
            if ssh_path.exists() {
                let result = validate_deep_link_cwd(&ssh_path, &[]);
                assert!(
                    matches!(result, Err(CwdValidationError::Denied(_))),
                    "~/.ssh should be denied: {result:?}"
                );
            } else {
                let denylist = build_denylist();
                let ssh_entry = format!("{home}/.ssh").to_lowercase();
                assert!(
                    denylist.iter().any(|d| d == &ssh_entry),
                    "~/.ssh should be in denylist; got {denylist:?}"
                );
            }
        }
    }

    #[test]
    fn denylist_false_positive_processing() {
        // /processing must NOT be blocked by the /proc entry.
        let denylist = build_denylist();
        let candidate = "/processing";
        for entry in &denylist {
            let is_exact = candidate == entry.as_str();
            let is_prefix = candidate.starts_with(&format!("{entry}/"));
            assert!(
                !is_exact && !is_prefix,
                "/processing should not be denied by denylist entry {entry:?}"
            );
        }
    }
}
