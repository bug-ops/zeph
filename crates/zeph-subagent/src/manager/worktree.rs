// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

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
}

impl Drop for WorktreeCleanupGuard {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        let wm = Arc::clone(&self.wm);
        let h = self.handle.clone();
        let prune = self.prune;
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    if let Err(e) = wm.remove(&h, prune).await {
                        tracing::warn!(error = %e, "failed to remove sub-agent worktree");
                    }
                });
            }
            Err(_) => {
                tracing::error!(
                    "no tokio runtime in WorktreeCleanupGuard::drop — worktree cleanup skipped"
                );
            }
        }
    }
}
