// SPDX-License-Identifier: MIT
//! [`WorktreeHandle`] — a live record of a managed git worktree.

use std::{path::PathBuf, time::SystemTime};

/// Sentinel used for [`WorktreeHandle::branch_name`] when a worktree discovered
/// via [`WorktreeManager::reconcile`][crate::WorktreeManager::reconcile] is on a
/// detached `HEAD` rather than a branch (`git worktree list --porcelain` emits a
/// `detached` line instead of `branch refs/heads/<name>` for these entries).
///
/// The embedded space makes this an invalid git ref name: `git
/// check-ref-format` rejects any ref component containing a space (see
/// `git help check-ref-format` — disallowed characters include space, `~`,
/// `^`, `:`, `?`, `*`, `[`, `\`). `reconcile()` only ever populates
/// `branch_name` from a `branch refs/heads/<name>` porcelain line, and git
/// itself refuses to create a ref containing a space in the first place — so
/// no real branch, including one on a worktree foreign to zeph (i.e. not
/// created via [`WorktreeManager::create`][crate::WorktreeManager::create],
/// which further restricts the subagent-id component to
/// `^[A-Za-z0-9._-]+$`), can ever equal this sentinel. An earlier version of
/// this constant, `"(detached)"`, lacked this property: parentheses are
/// valid in git ref names, so a real branch literally named `(detached)`
/// would have been indistinguishable from a detached-HEAD worktree, causing
/// [`WorktreeManager::remove`][crate::WorktreeManager::remove] to silently
/// skip pruning it (#5936 review finding).
pub const DETACHED_BRANCH_SENTINEL: &str = "(detached HEAD)";

/// A live record of a git worktree that [`WorktreeManager`][crate::WorktreeManager]
/// has created for a subagent.
///
/// Handles are stored in-memory for the duration of the session.  They are not
/// persisted to disk; on restart, [`WorktreeManager::reconcile`][crate::WorktreeManager::reconcile]
/// re-discovers handles from the git worktree registry.
#[derive(Debug, Clone)]
pub struct WorktreeHandle {
    /// Absolute path on disk where the worktree was checked out.
    pub path: PathBuf,
    /// The git branch name created for this worktree.
    ///
    /// For worktrees discovered by [`WorktreeManager::reconcile`][crate::WorktreeManager::reconcile]
    /// that are on a detached `HEAD`, this is [`DETACHED_BRANCH_SENTINEL`] rather
    /// than an actual branch name.
    pub branch_name: String,
    /// The resolved base ref used to create the branch.
    ///
    /// `"HEAD"` for [`WorktreeBaseRef::Head`][zeph_config::WorktreeBaseRef::Head],
    /// `"origin/{branch}"` for [`WorktreeBaseRef::Fresh`][zeph_config::WorktreeBaseRef::Fresh].
    pub base_ref_resolved: String,
    /// The subagent identifier that this worktree was created for.
    pub subagent_id: String,
    /// Wall-clock time when the worktree was created.
    pub created_at: SystemTime,
}
