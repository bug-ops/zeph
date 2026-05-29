// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Process-level working-directory guard for worktree-isolated sub-agents.
//!
//! [`CwdRestoreGuard`] combines two responsibilities:
//!
//! 1. **Serialisation** — it holds an [`OwnedMutexGuard`] that prevents any other agent
//!    from mutating or observing the process cwd while this guard is alive (INV-1).
//! 2. **Restore-on-drop** — `Drop` calls [`std::env::set_current_dir`] back to the
//!    pre-spawn directory *before* releasing the mutex, guaranteeing that once the guard
//!    drops the cwd is clean for the next agent.
//!
//! # Drop ordering
//!
//! Rust drops struct fields in declaration order.  `prev_cwd` is declared first so the
//! cwd-restore logic in `Drop` runs before `_guard` drops (releasing the mutex).  This
//! means the mutex is still held during the restore, preventing a race window where another
//! task could observe a partially-restored cwd.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, OwnedMutexGuard};

/// RAII guard that restores the process working directory on drop and holds a
/// process-level serialisation mutex for the duration of a sub-agent run.
///
/// Create via [`CwdRestoreGuard::new`] or obtain an unmodified guard (no cwd change)
/// via [`CwdRestoreGuard::acquire`].
///
/// # Examples
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use tokio::sync::Mutex;
/// use zeph_subagent::cwd_guard::CwdRestoreGuard;
///
/// # async fn example() -> std::io::Result<()> {
/// let lock = Arc::new(Mutex::new(()));
/// let guard = lock.clone().lock_owned().await;
/// let _g = CwdRestoreGuard::acquire(guard);
/// // cwd is unchanged; mutex held until _g drops
/// # Ok(())
/// # }
/// ```
pub struct CwdRestoreGuard {
    /// Directory to restore on drop; always set, even for plain-agent guards.
    prev_cwd: PathBuf,
    /// Held for the full lifetime of the guard; released after cwd is restored.
    _guard: OwnedMutexGuard<()>,
}

impl CwdRestoreGuard {
    /// Saves the current cwd, changes to `new_cwd`, and holds `guard`.
    ///
    /// # Errors
    ///
    /// Returns an [`std::io::Error`] if [`std::env::current_dir`] or
    /// [`std::env::set_current_dir`] fails.
    pub fn new(new_cwd: impl Into<PathBuf>, guard: OwnedMutexGuard<()>) -> std::io::Result<Self> {
        let prev_cwd = std::env::current_dir()?;
        std::env::set_current_dir(new_cwd.into())?;
        Ok(Self {
            prev_cwd,
            _guard: guard,
        })
    }

    /// Acquires the guard without changing the cwd (used for plain-agent serialisation).
    ///
    /// The current directory is saved so that `Drop` is a no-op restore (idempotent).
    ///
    /// # Errors
    ///
    /// Returns an [`std::io::Error`] if [`std::env::current_dir`] fails.
    pub fn acquire(guard: OwnedMutexGuard<()>) -> std::io::Result<Self> {
        let prev_cwd = std::env::current_dir()?;
        Ok(Self {
            prev_cwd,
            _guard: guard,
        })
    }
}

impl Drop for CwdRestoreGuard {
    fn drop(&mut self) {
        // Restore cwd before _guard drops so no other task can observe a stale cwd.
        if let Err(e) = std::env::set_current_dir(&self.prev_cwd) {
            tracing::error!(
                dir = %self.prev_cwd.display(),
                error = %e,
                "failed to restore process cwd"
            );
        }
    }
}

/// Convenience type alias for the shared mutex used as the process-level cwd lock.
pub type CwdLock = Arc<Mutex<()>>;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serial_test::serial;
    use tokio::sync::{Barrier, Mutex};

    use super::{CwdLock, CwdRestoreGuard};

    /// Drop restores the cwd when a worktree guard goes out of scope.
    #[tokio::test]
    #[serial]
    async fn drop_restores_cwd() {
        let original = std::env::current_dir().unwrap().canonicalize().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let lock: CwdLock = Arc::new(Mutex::new(()));

        {
            let guard = lock.clone().lock_owned().await;
            let _g = CwdRestoreGuard::new(tmp.path(), guard).unwrap();
            assert_eq!(
                std::env::current_dir().unwrap().canonicalize().unwrap(),
                tmp.path().canonicalize().unwrap()
            );
        }

        assert_eq!(
            std::env::current_dir().unwrap().canonicalize().unwrap(),
            original
        );
    }

    /// Drop of an acquire-only guard is a no-op cwd change but still restores.
    #[tokio::test]
    #[serial]
    async fn acquire_only_restores_cwd() {
        let original = std::env::current_dir().unwrap();
        let lock: CwdLock = Arc::new(Mutex::new(()));

        {
            let guard = lock.clone().lock_owned().await;
            let _g = CwdRestoreGuard::acquire(guard).unwrap();
        }

        assert_eq!(
            std::env::current_dir().unwrap().canonicalize().unwrap(),
            original.canonicalize().unwrap()
        );
    }

    /// M4 concurrency test: a second task cannot acquire the guard while the first holds it.
    ///
    /// Uses a `Barrier` and a `oneshot` channel to deterministically verify ordering
    /// without sleeps, per critic NR-1.
    #[tokio::test]
    #[serial]
    async fn m4_plain_agent_blocks_while_worktree_guard_held() {
        let lock: CwdLock = Arc::new(Mutex::new(()));

        // Barrier: two parties — first task signals "guard held", test waits.
        let barrier = Arc::new(Barrier::new(2));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel::<()>();

        let lock1 = Arc::clone(&lock);
        let b1 = Arc::clone(&barrier);
        let holder = tokio::spawn(async move {
            let guard = lock1.clone().lock_owned().await;
            let _g = CwdRestoreGuard::acquire(guard).unwrap();
            // Signal: guard is held.
            let _ = acquired_tx.send(());
            // Wait for the test to confirm the second task is blocked, then release.
            b1.wait().await;
            let _ = release_rx.await;
            // _g drops here, releasing the mutex.
        });

        // Wait until the first task has acquired the guard.
        acquired_rx.await.unwrap();

        // Now attempt to acquire in a second task — it must block.
        // Use a Semaphore with 0 permits so the waiter task can signal when it
        // *attempts* acquisition. We verify it hasn't completed before we release.
        let lock2 = Arc::clone(&lock);
        let (second_acquired_tx, second_acquired_rx) = tokio::sync::oneshot::channel::<()>();
        // A semaphore the waiter adds a permit to once it has acquired the guard.
        let waiter_done = Arc::new(tokio::sync::Semaphore::new(0));
        let waiter_done2 = Arc::clone(&waiter_done);
        let waiter = tokio::spawn(async move {
            let guard = lock2.clone().lock_owned().await;
            let _ = CwdRestoreGuard::acquire(guard);
            let _ = second_acquired_tx.send(());
            waiter_done2.add_permits(1);
        });

        // Yield to allow the waiter task to be scheduled and attempt acquisition.
        // tokio::task::yield_now() gives up the current timeslice so the waiter
        // reaches lock2.lock_owned().await and blocks there.
        tokio::task::yield_now().await;
        // The waiter should still be blocked (no permits available).
        assert!(
            waiter_done.try_acquire().is_err(),
            "second task must not acquire the guard while first holds it"
        );

        // Release the first guard.
        barrier.wait().await;
        let _ = release_tx.send(());
        holder.await.unwrap();

        // Now the second task should complete.
        waiter.await.unwrap();
        assert!(
            second_acquired_rx.await.is_ok(),
            "second task should have acquired the guard after first released it"
        );
    }
}
