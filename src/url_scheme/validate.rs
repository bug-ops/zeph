// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! CWD path validation for `zeph://` deep links.
//!
//! The implementation lives in `zeph-common` so it can be shared with the ACP HTTP
//! transport. This module re-exports the shared types for use by the CLI entry point.

pub use zeph_common::deep_link::validate_deep_link_cwd;

#[cfg(test)]
use zeph_common::deep_link::{CwdValidationError, build_cwd_denylist};

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

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
                let denylist = build_cwd_denylist();
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
        let denylist = build_cwd_denylist();
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
