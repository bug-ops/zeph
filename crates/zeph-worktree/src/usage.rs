// SPDX-License-Identifier: MIT
//! Disk-usage accounting and quota-status types for [`crate::WorktreeManager`].

use std::fmt::Write as _;
use std::path::PathBuf;

use zeph_config::WorktreeConfig;

/// Total and per-worktree disk usage, as computed by
/// [`WorktreeManager::disk_usage`][crate::WorktreeManager::disk_usage].
///
/// `total_bytes` is a sum of logical file sizes (`std::fs::Metadata::len`) across
/// every regular file under each worktree's directory tree — not on-disk block
/// usage. Content shared via hardlinks across worktrees (e.g. zeph-session blobs)
/// can be double-counted, so treat this as an approximation suitable for a soft
/// warn threshold, not exact `du` output.
///
/// # Examples
///
/// ```
/// use zeph_worktree::WorktreeDiskUsage;
///
/// let usage = WorktreeDiskUsage {
///     total_bytes: 1024,
///     per_worktree: vec![(std::path::PathBuf::from("/repo/worktrees/agent-1"), 1024)],
/// };
/// assert_eq!(usage.total_bytes, 1024);
/// ```
#[derive(Debug, Clone, Default)]
pub struct WorktreeDiskUsage {
    /// Sum of logical file sizes across every worktree under `root`.
    pub total_bytes: u64,
    /// Per-worktree logical size, in the same order `reconcile()` + `list()`
    /// returned them.
    pub per_worktree: Vec<(PathBuf, u64)>,
}

/// Result of one reconcile-and-quota sweep, as returned by
/// [`WorktreeManager::sweep`][crate::WorktreeManager::sweep].
///
/// Carries enough information for a caller to decide whether to surface a status
/// indicator: [`QuotaStatus::is_over_quota`] is `true` when either the count or
/// disk-usage threshold configured in `WorktreeConfig` was exceeded at sweep time.
///
/// # Examples
///
/// ```
/// use zeph_worktree::QuotaStatus;
///
/// let status = QuotaStatus {
///     count: 3,
///     max_worktrees: Some(5),
///     total_bytes: 1_000_000,
///     disk_quota_bytes: None,
///     reclaimed: 0,
///     over_count: false,
///     over_disk: false,
/// };
/// assert!(!status.is_over_quota());
/// ```
#[derive(Debug, Clone, Default)]
pub struct QuotaStatus {
    /// Number of git-registered secondary worktrees under `root` after reclamation.
    pub count: usize,
    /// The configured `worktree.max_worktrees` limit, if set.
    pub max_worktrees: Option<usize>,
    /// Total logical disk usage across all worktrees, in bytes, after reclamation.
    ///
    /// `0` when disk accounting was not performed for this sweep (see
    /// [`WorktreeManager::sweep`][crate::WorktreeManager::sweep]'s use of
    /// `config.disk_quota_mb` to decide whether to walk) — callers must check
    /// `disk_quota_bytes.is_some()` before treating `0` as a meaningful "no usage" result.
    pub total_bytes: u64,
    /// The configured `worktree.disk_quota_mb` limit converted to bytes, if set.
    pub disk_quota_bytes: Option<u64>,
    /// Number of `prunable` entries automatically removed during this sweep.
    pub reclaimed: usize,
    /// `true` when `count >= max_worktrees` (only meaningful if `max_worktrees` is `Some`).
    pub over_count: bool,
    /// `true` when `total_bytes >= disk_quota_bytes` (only meaningful if
    /// `disk_quota_bytes` is `Some`).
    pub over_disk: bool,
}

impl QuotaStatus {
    /// `true` if either the worktree-count or disk-usage threshold was exceeded.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_worktree::QuotaStatus;
    ///
    /// let status = QuotaStatus {
    ///     over_count: true,
    ///     ..Default::default()
    /// };
    /// assert!(status.is_over_quota());
    /// ```
    #[must_use]
    pub fn is_over_quota(&self) -> bool {
        self.over_count || self.over_disk
    }
}

/// Formats a one-line disk-usage-and-quota summary, shared by the CLI's
/// `zeph worktree list` (`src/commands/worktree.rs`) and the agent-side `/worktree list`
/// slash command (`crates/zeph-core/src/agent/worktree_commands.rs`) so the two surfaces
/// cannot silently diverge in how they report quota status.
///
/// `count` is the total number of git-registered secondary worktrees under `root`
/// (active + stale), matching the same count [`WorktreeManager::create`][crate::WorktreeManager::create]
/// and [`WorktreeManager::sweep`][crate::WorktreeManager::sweep] use for admission and
/// quota evaluation.
///
/// # Examples
///
/// ```
/// use zeph_config::WorktreeConfig;
/// use zeph_worktree::{WorktreeDiskUsage, format_usage_summary};
///
/// let usage = WorktreeDiskUsage { total_bytes: 2 * 1_048_576, per_worktree: Vec::new() };
/// let config = WorktreeConfig { max_worktrees: Some(5), disk_quota_mb: Some(1), ..Default::default() };
/// let summary = format_usage_summary(&usage, 2, &config);
/// assert!(summary.contains("OVER"), "got: {summary}");
/// ```
#[must_use]
pub fn format_usage_summary(
    usage: &WorktreeDiskUsage,
    count: usize,
    config: &WorktreeConfig,
) -> String {
    let used_mb = usage.total_bytes / 1_048_576;
    let mut line = format!("{count} worktree(s), {used_mb} MB");

    if let Some(max) = config.max_worktrees {
        let status = if count >= max { "OVER" } else { "ok" };
        let _ = write!(line, ", max_worktrees: {count}/{max} [{status}]");
    }
    if let Some(quota_mb) = config.disk_quota_mb {
        let status = if used_mb >= quota_mb { "OVER" } else { "ok" };
        let _ = write!(line, ", disk_quota_mb: {used_mb}/{quota_mb} [{status}]");
    }

    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_over_quota_true_when_over_count() {
        let status = QuotaStatus {
            over_count: true,
            ..Default::default()
        };
        assert!(status.is_over_quota());
    }

    #[test]
    fn is_over_quota_true_when_over_disk() {
        let status = QuotaStatus {
            over_disk: true,
            ..Default::default()
        };
        assert!(status.is_over_quota());
    }

    #[test]
    fn is_over_quota_false_when_neither() {
        assert!(!QuotaStatus::default().is_over_quota());
    }

    #[test]
    fn format_usage_summary_no_quota_configured() {
        let usage = WorktreeDiskUsage {
            total_bytes: 5 * 1_048_576,
            per_worktree: Vec::new(),
        };
        let summary = format_usage_summary(&usage, 3, &WorktreeConfig::default());
        assert_eq!(summary, "3 worktree(s), 5 MB");
    }

    #[test]
    fn format_usage_summary_reports_ok_under_thresholds() {
        let usage = WorktreeDiskUsage {
            total_bytes: 1_048_576,
            per_worktree: Vec::new(),
        };
        let config = WorktreeConfig {
            max_worktrees: Some(5),
            disk_quota_mb: Some(10),
            ..Default::default()
        };
        let summary = format_usage_summary(&usage, 1, &config);
        assert!(
            summary.contains("max_worktrees: 1/5 [ok]"),
            "got: {summary}"
        );
        assert!(
            summary.contains("disk_quota_mb: 1/10 [ok]"),
            "got: {summary}"
        );
        assert!(!summary.contains("OVER"), "got: {summary}");
    }

    #[test]
    fn format_usage_summary_reports_over_at_thresholds() {
        let usage = WorktreeDiskUsage {
            total_bytes: 10 * 1_048_576,
            per_worktree: Vec::new(),
        };
        let config = WorktreeConfig {
            max_worktrees: Some(2),
            disk_quota_mb: Some(10),
            ..Default::default()
        };
        let summary = format_usage_summary(&usage, 2, &config);
        assert!(
            summary.contains("max_worktrees: 2/2 [OVER]"),
            "got: {summary}"
        );
        assert!(
            summary.contains("disk_quota_mb: 10/10 [OVER]"),
            "got: {summary}"
        );
    }
}
