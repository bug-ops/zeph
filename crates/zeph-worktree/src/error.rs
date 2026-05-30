// SPDX-License-Identifier: MIT
//! Error types for the `zeph-worktree` crate.

use std::path::PathBuf;

#[non_exhaustive]
/// All errors that `zeph-worktree` can produce.
///
/// Every variant is designed so that the `Display` message is safe to show to the
/// user; raw git stderr is kept in [`WorktreeError::GitCommand`]'s `stderr` field
/// and must only be logged at `DEBUG` level — never forwarded to the user.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    /// The working directory is not inside a git repository.
    #[error("not inside a git repository")]
    NotAGitRepo,

    /// A git sub-command exited with a non-zero status.
    ///
    /// `op` names the operation (e.g. `"fetch"`, `"worktree add"`).
    /// `stderr` is raw git output — log at `DEBUG`, never surface to the user.
    #[error("git command `{op}` failed")]
    GitCommand {
        /// The git operation that failed (e.g. `"fetch"`, `"worktree add"`).
        op: String,
        /// Raw stderr from git — for diagnostic logging only.
        stderr: String,
    },

    /// The computed worktree path already exists on disk.
    #[error("worktree path already exists: {0}")]
    PathExists(PathBuf),

    /// The default branch could not be resolved.
    ///
    /// Emitted when `config.default_branch` is empty and
    /// `git symbolic-ref refs/remotes/origin/HEAD` fails.
    #[error("cannot resolve default branch: attempted {attempted}")]
    BaseRefUnresolved {
        /// Description of what was attempted before giving up.
        attempted: String,
    },

    /// The `subagent_id` value contains characters that are not allowed in a
    /// git branch component.
    #[error("invalid branch name component: {0}")]
    InvalidBranchName(String),

    /// The canonicalised worktree root resolves to a path outside the repository.
    #[error("worktree root resolves outside the repository: {0}")]
    RootOutsideRepo(PathBuf),

    /// An I/O error propagated from the OS or `std::fs`.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
