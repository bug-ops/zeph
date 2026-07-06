// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use zeph_common::TaskSupervisor;

/// RAII guard that calls [`DefaultWorktreeManager::remove`] when dropped.
///
/// Guarantees cleanup on normal return and on early `?` returns from the agent loop.
/// With `panic = "abort"` (release profile) panics terminate the process via `abort(3)`
/// before any `Drop` runs; the OS reclaims resources directly. With `panic = "unwind"`
/// (dev/test profile) `Drop` runs during stack unwinding.
pub(crate) struct WorktreeCleanupGuard {
    pub(crate) wm: Arc<zeph_worktree::DefaultWorktreeManager>,
    pub(crate) handle: zeph_worktree::WorktreeHandle,
    pub(crate) prune: bool,
    pub(crate) enabled: bool,
    /// Session [`TaskSupervisor`], sourced from [`SubAgentManager`][super::SubAgentManager],
    /// so the cleanup task is registered and visible like any other supervised task.
    ///
    /// `None` (e.g. in unit tests that build the guard directly) falls back to a transient
    /// local supervisor, mirroring [`SubAgentManager::spawn_agent_task`][super::SubAgentManager::spawn_agent_task].
    pub(crate) task_supervisor: Option<TaskSupervisor>,
}

impl Drop for WorktreeCleanupGuard {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::error!(
                "no tokio runtime in WorktreeCleanupGuard::drop — worktree cleanup skipped"
            );
            return;
        }
        let wm = Arc::clone(&self.wm);
        let h = self.handle.clone();
        let prune = self.prune;
        let name: Arc<str> = Arc::from(format!("worktree-cleanup-{}", h.subagent_id));
        let factory = move || async move {
            if let Err(e) = wm.remove(&h, prune).await {
                tracing::warn!(error = %e, "failed to remove sub-agent worktree");
            }
        };
        match &self.task_supervisor {
            Some(sup) => {
                sup.spawn_oneshot(name, factory);
            }
            None => {
                TaskSupervisor::new(CancellationToken::new()).spawn_oneshot(name, factory);
            }
        }
    }
}
