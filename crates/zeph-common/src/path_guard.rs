// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Classifier for the "relative paths only, no `..`" policy used by every
//! attachment-loading entry point that has no sandbox/`allowed_paths` configuration
//! available to it (currently `/image`).
//!
//! This is deliberately **not** a sandbox: it has no notion of allowed roots and does
//! not canonicalize or touch the filesystem. It only classifies a path string as
//! absolute, `..`-traversing, or acceptable, so every caller enforcing "relative paths
//! only" applies the exact same rule and reports the exact same classification —
//! closing the drift risk of three independent, textually-similar-but-not-identical
//! checks (one such check previously missed the Windows leading-`/` guard the other
//! two had).

use std::path::{Component, Path};

/// Outcome of classifying a path against the "relative, no `..`" policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRejection {
    /// Path is relative and contains no `..` component — acceptable.
    Allowed,
    /// Path is absolute, or looks absolute via a leading `/`.
    ///
    /// `Path::is_absolute` is `false` on Windows for Unix-style paths like `/etc/passwd`
    /// (no drive letter), so the leading-`/` string check is required in addition to it.
    Absolute,
    /// Path contains a `..` (`Component::ParentDir`) component.
    Traversal,
}

/// Classify `path` against the "relative paths only, no `..`" policy.
///
/// Checks the absolute case before the traversal case, so a path that is both absolute
/// and contains `..` is reported as [`PathRejection::Absolute`].
///
/// # Examples
///
/// ```
/// use zeph_common::path_guard::{PathRejection, classify_relative_path};
///
/// assert_eq!(classify_relative_path("photo.jpg"), PathRejection::Allowed);
/// assert_eq!(classify_relative_path("/etc/passwd"), PathRejection::Absolute);
/// assert_eq!(classify_relative_path("../../etc/passwd"), PathRejection::Traversal);
/// ```
#[must_use]
pub fn classify_relative_path(path: &str) -> PathRejection {
    let p = Path::new(path);
    if p.is_absolute() || path.starts_with('/') {
        return PathRejection::Absolute;
    }
    if p.components().any(|c| c == Component::ParentDir) {
        return PathRejection::Traversal;
    }
    PathRejection::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_plain_relative_path() {
        assert_eq!(classify_relative_path("photo.jpg"), PathRejection::Allowed);
    }

    #[test]
    fn allows_nested_relative_path() {
        assert_eq!(
            classify_relative_path("sub/dir/photo.jpg"),
            PathRejection::Allowed
        );
    }

    #[test]
    fn rejects_absolute_unix_path() {
        assert_eq!(
            classify_relative_path("/etc/passwd"),
            PathRejection::Absolute
        );
    }

    #[test]
    fn rejects_leading_slash_even_when_is_absolute_would_miss_it() {
        // A leading '/' must be caught even on platforms where `Path::is_absolute()`
        // alone would not flag it (Windows).
        assert_eq!(
            classify_relative_path("/tmp/screenshot.png"),
            PathRejection::Absolute
        );
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        assert_eq!(
            classify_relative_path("../../etc/passwd"),
            PathRejection::Traversal
        );
    }

    #[test]
    fn rejects_nested_parent_dir_traversal() {
        assert_eq!(
            classify_relative_path("sub/../../etc/passwd"),
            PathRejection::Traversal
        );
    }

    #[test]
    fn absolute_takes_priority_over_traversal() {
        assert_eq!(
            classify_relative_path("/etc/../etc/passwd"),
            PathRejection::Absolute
        );
    }

    #[test]
    fn allows_single_dot_component() {
        assert_eq!(
            classify_relative_path("./photo.jpg"),
            PathRejection::Allowed
        );
    }
}
