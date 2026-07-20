// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`WorktreeAccess`] — command handler access to the live session's git worktree manager
//! and the session working directory.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to `/worktree` and `/cd`.
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait WorktreeAccess {
    // ----- /worktree -----

    /// Return a formatted list of active and stale git worktrees tracked by the live
    /// session's worktree manager, or `None` when the worktree subsystem is disabled.
    ///
    /// Used by `/worktree` and `/worktree list`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the underlying git reconciliation fails.
    fn list_worktrees<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>> {
        Box::pin(async { Ok(None) })
    }

    /// Remove stale worktrees tracked by the live session's worktree manager.
    ///
    /// `force` mirrors `zeph worktree clean --force`: also removes worktrees whose
    /// directory git does not report as prunable. Returns `None` when the worktree
    /// subsystem is disabled.
    ///
    /// Used by `/worktree clean`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the underlying git reconciliation or registry pruning fails.
    fn clean_worktrees<'a>(
        &'a mut self,
        force: bool,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>> {
        let _ = force;
        Box::pin(async { Ok(None) })
    }

    // ----- /cd -----

    /// Change the session's primary working directory, or report the current one when
    /// `path` is empty (#6032, FR-009).
    ///
    /// Reuses `zeph_tools::resolve_and_set_cwd` — the same path-resolution logic the
    /// LLM-invoked `set_working_directory` tool uses — then runs the agent's
    /// `check_cwd_changed` post-change pipeline (repo-map invalidation, `cwd_changed` hooks,
    /// and — unless the session is in `--safe-mode` — CLAUDE.md/AGENTS.md instruction
    /// re-discovery). `/cd` is an additive user-facing entry point into that existing
    /// mechanism, not a parallel implementation.
    ///
    /// Returns a confirmation string with the new (or current) absolute working directory.
    ///
    /// # Errors
    ///
    /// Returns `Err` when `path` does not resolve to an existing, readable directory.
    fn change_working_directory<'a>(
        &'a mut self,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let _ = path;
        Box::pin(async move { Err(CommandError::new("/cd is not supported in this context")) })
    }
}
