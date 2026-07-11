// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-subagent git worktree lifecycle management for Zeph.
//!
//! This crate implements the `zeph-worktree` subsystem described in
//! `specs/063-worktree-subsystem/spec.md`.  It provides:
//!
//! - [`WorktreeManager`] — creates, removes, lists, and reconciles git worktrees
//! - [`WorktreeHandle`] — a live record of one managed worktree
//! - [`StaleWorktree`] — a worktree discovered by `reconcile` outside session state,
//!   annotated with git's own `prunable` verdict
//! - [`WorktreeError`] — all errors this crate can produce
//! - [`GitRunner`] / [`DefaultGitRunner`]
//!   — the git invocation abstraction and its production implementation
//! - [`manager::probe_capabilities`] — bootstrap git availability probe
//!
//! ## Dependency direction
//!
//! ```text
//! zeph-subagent → zeph-worktree → (zeph-config, tokio, thiserror, tracing)
//! ```
//!
//! This crate MUST NOT depend on `zeph-core`, `zeph-subagent`, or
//! `zeph-channels`.
//!
//! ## Example
//!
//! ```no_run
//! use std::path::PathBuf;
//! use zeph_config::WorktreeConfig;
//! use zeph_worktree::{DefaultWorktreeManager, git_runner::DefaultGitRunner, manager::probe_capabilities};
//!
//! # async fn example() -> Result<(), zeph_worktree::WorktreeError> {
//! let repo = PathBuf::from("/path/to/repo");
//! let runner = DefaultGitRunner::new();
//! probe_capabilities(&runner, &repo).await?;
//!
//! let mgr = DefaultWorktreeManager::new(repo, WorktreeConfig::default(), DefaultGitRunner::new()).await?;
//! let handle = mgr.create("agent-42").await?;
//! println!("Worktree at {:?}", handle.path);
//! mgr.remove(&handle, false).await?;
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod git_runner;
pub mod handle;
pub mod manager;
pub mod sanitize;

pub use error::WorktreeError;
pub use git_runner::{DefaultGitRunner, GitRunner};
pub use handle::{BARE_WORKTREE_SENTINEL, DETACHED_BRANCH_SENTINEL, StaleWorktree, WorktreeHandle};
pub use manager::{WorktreeManager, probe_capabilities};

/// A [`WorktreeManager`] using the production [`DefaultGitRunner`].
///
/// This is the type that `SubAgentManager` stores as
/// `Option<Arc<DefaultWorktreeManager>>`.
pub type DefaultWorktreeManager = WorktreeManager<DefaultGitRunner>;
