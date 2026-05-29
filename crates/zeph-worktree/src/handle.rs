// SPDX-License-Identifier: MIT
//! [`WorktreeHandle`] — a live record of a managed git worktree.

use std::{path::PathBuf, time::SystemTime};

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
