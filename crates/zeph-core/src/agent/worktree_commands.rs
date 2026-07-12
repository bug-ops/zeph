// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Channel-free `/worktree` command implementation for use via
//! [`zeph_commands::traits::agent::AgentAccess`].
//!
//! Operates on the same live [`zeph_worktree::DefaultWorktreeManager`] instance the running
//! agent's [`zeph_subagent::SubAgentManager`] uses to create per-subagent worktrees, so
//! `/worktree list` and `/worktree clean` reflect this session's actual state. Contrast with
//! the CLI's `zeph worktree list`/`clean` (`src/commands/worktree.rs`), which constructs a
//! fresh manager from a disk scan on every invocation.

use std::fmt::Write as _;

use super::{Agent, error::AgentError};
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Channel-free `/worktree list` — formats active and stale worktrees tracked by the
    /// live session's worktree manager.
    ///
    /// Returns `Ok(None)` when the worktree subsystem is disabled for this session.
    ///
    /// # Errors
    ///
    /// Returns `Err` when git reconciliation fails.
    pub(super) async fn handle_worktree_list_as_string(
        &mut self,
    ) -> Result<Option<String>, AgentError> {
        let Some(mgr) = &self.services.orchestration.subagent_manager else {
            return Ok(None);
        };
        let Some(wm) = mgr.worktree_manager() else {
            return Ok(None);
        };

        let stale = wm.reconcile().await?;
        let active = wm.list();

        if active.is_empty() && stale.is_empty() {
            return Ok(Some("No active worktrees.".to_owned()));
        }

        let mut out = String::new();
        if !active.is_empty() {
            let _ = writeln!(out, "{:<36}  PATH", "AGENT ID");
            for handle in &active {
                let _ = writeln!(out, "{:<36}  {}", handle.subagent_id, handle.path.display());
            }
        }
        if !stale.is_empty() {
            if !active.is_empty() {
                out.push('\n');
            }
            out.push_str("Stale (on disk but not tracked):\n");
            for stale_wt in &stale {
                match &stale_wt.prunable_reason {
                    Some(reason) => {
                        let _ = writeln!(
                            out,
                            "  {}  [prunable: {reason}]",
                            stale_wt.handle.path.display()
                        );
                    }
                    None => {
                        let _ = writeln!(
                            out,
                            "  {}  [in use — not marked prunable by git; may belong to \
                             another session]",
                            stale_wt.handle.path.display()
                        );
                    }
                }
            }
        }
        Ok(Some(out.trim_end().to_owned()))
    }

    /// Channel-free `/worktree clean [--force]` — removes stale worktrees tracked by the
    /// live session's worktree manager.
    ///
    /// `force` mirrors `zeph worktree clean --force`: also removes worktrees whose directory
    /// git does not report as prunable. Returns `Ok(None)` when the worktree subsystem is
    /// disabled for this session.
    ///
    /// # Errors
    ///
    /// Returns `Err` only when the initial git reconciliation fails (nothing has been
    /// removed yet, so there is no summary to lose). Per-worktree removal failures and a
    /// failure of the final registry-prune step are both reported inline in the summary
    /// instead of aborting, matching the CLI's `zeph worktree clean` behavior — a prune
    /// failure must not discard an otherwise-successful removal summary.
    pub(super) async fn handle_worktree_clean_as_string(
        &mut self,
        force: bool,
    ) -> Result<Option<String>, AgentError> {
        let Some(mgr) = &self.services.orchestration.subagent_manager else {
            return Ok(None);
        };
        let Some(wm) = mgr.worktree_manager() else {
            return Ok(None);
        };
        let prune_branch_on_remove = wm.prune_branch_on_remove();

        let stale = wm.reconcile().await?;
        let mut removed = 0usize;
        let mut skipped = 0usize;
        let mut warnings = String::new();
        for stale_wt in stale {
            if !force && !stale_wt.is_safe_to_force_remove() {
                let _ = writeln!(
                    warnings,
                    "warning: skipping {} — directory exists and git does not report it as \
                     prunable; it may be in active use by another zeph session.",
                    stale_wt.handle.path.display()
                );
                skipped += 1;
                continue;
            }
            if let Err(e) = wm.remove(&stale_wt.handle, prune_branch_on_remove).await {
                let _ = writeln!(
                    warnings,
                    "warning: failed to remove {}: {e}",
                    stale_wt.handle.path.display()
                );
            } else {
                removed += 1;
            }
        }
        if let Err(e) = wm.prune().await {
            let _ = writeln!(warnings, "warning: failed to prune worktree registry: {e}");
        }

        let mut out = warnings;
        let _ = write!(
            out,
            "Removed {removed} stale worktree(s), skipped {skipped} in-use candidate(s)."
        );
        Ok(Some(out))
    }
}
