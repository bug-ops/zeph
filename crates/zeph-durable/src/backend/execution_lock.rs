// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cross-process advisory lock enforcing INV-15's single-owner-process invariant, held for the
//! lifetime of a [`crate::DurableContext`] opened via
//! [`LocalBackend::open_execution_exclusive`](crate::LocalBackend::open_execution_exclusive).
//!
//! Two processes can independently derive the same [`ExecutionId`] — the P1 agent-turn adapter
//! keys it on `(ConversationId, sqlite_path)`, so two CLI instances pointed at the same memory
//! database and the same conversation always agree on the id (#6122). Without a lock, both
//! processes race `LocalBackend::open_execution`'s plain SELECT-then-INSERT and both drive
//! `next_step` from 0, corrupting the journal (`ReplayDivergence`/`ReplayIntegrity` on whichever
//! process loses the race). [`ExecutionLock`] closes that race with a non-blocking, exclusive
//! `flock(2)` on a lock file named after the execution's UUID.
//!
//! # Why not `zeph_common::pidfile::PidLockGuard`
//!
//! `PidLockGuard` (the primitive backing `zeph-core::daemon::PidGuard` and
//! `zeph-scheduler::pidfile::PidFile`) unlinks its lock file *before* its file descriptor closes
//! (see its `Drop` impl). That ordering is safe for a pid file acquired once at daemon startup and
//! released once at shutdown — vanishingly unlikely to race a concurrent acquirer — but is a real
//! `flock`+`unlink` TOCTOU hazard for a lock acquired and released once per conversation turn under
//! real contention (e.g. many CI agents sharing a testing database): a second process racing the
//! unlink window can `open(O_CREAT)` a *fresh* inode at the just-unlinked path and `flock` it
//! immediately, believing it holds the lock, while the first process's descriptor — still open on
//! the now-orphaned original inode — has not actually released yet. [`ExecutionLock`] instead
//! mirrors `zeph-session::log::SessionEventLog`'s own `AdvisoryLock`: the lock file is **never**
//! unlinked. It is a permanent sentinel, and the kernel releasing the `flock` when the holding
//! process's descriptors close (including on `SIGKILL`) is the only correctness signal — no
//! unlink/re-create race, no PID-liveness polling needed.

use std::path::Path;

use crate::error::DurableError;
use crate::ids::ExecutionId;

/// Holds the advisory lock for one [`ExecutionId`] while alive; releases it on drop.
///
/// Unix only. On non-Unix targets `ExecutionLock::acquire` always succeeds and returns a
/// no-op guard — the workspace has no vetted cross-platform advisory-locking primitive, mirroring
/// `SessionEventLog::open_exclusive`'s degrade.
#[cfg(unix)]
#[derive(Debug)]
pub struct ExecutionLock(#[allow(dead_code)] rustix::fd::OwnedFd);

#[cfg(unix)]
impl ExecutionLock {
    /// Acquire the exclusive lock for `id` under `lock_dir` (created on demand).
    ///
    /// Stamps the current process id into the lock file's content (best-effort, diagnostic only —
    /// never load-bearing for correctness) so a contending process can report a useful
    /// `holder_pid` in [`DurableError::ExecutionLocked`]; unlike `PidLockGuard`, the file is never
    /// unlinked, so a stale/unreadable pid on an old sentinel just yields `holder_pid: 0`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::ExecutionLocked`] if another process already holds the lock, or
    /// [`DurableError::Storage`] for any other filesystem failure.
    pub(crate) fn acquire(lock_dir: &Path, id: ExecutionId) -> Result<Self, DurableError> {
        use rustix::fs::{FlockOperation, Mode, OFlags};

        std::fs::create_dir_all(lock_dir)
            .map_err(|e| DurableError::storage("open_execution_exclusive", e))?;
        let lock_path = lock_dir.join(format!("{id}.lock"));

        let fd = rustix::fs::open(
            &lock_path,
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| DurableError::storage("open_execution_exclusive", std::io::Error::from(e)))?;

        rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                let holder_pid = zeph_common::pidfile::read_pid_lenient(&lock_path).unwrap_or(0);
                DurableError::ExecutionLocked {
                    execution_id: id,
                    holder_pid,
                }
            } else {
                DurableError::storage("open_execution_exclusive", std::io::Error::from(e))
            }
        })?;

        // Best-effort PID stamp for the next contender's error message — never propagate a
        // failure here, the lock itself is already held and correctness does not depend on it.
        let _ = rustix::fs::ftruncate(&fd, 0);
        let _ = rustix::io::write(&fd, std::process::id().to_string().as_bytes());

        Ok(Self(fd))
    }
}

/// No vetted cross-platform advisory-locking primitive exists in this workspace, so
/// [`LocalBackend::open_execution_exclusive`](crate::LocalBackend::open_execution_exclusive) does
/// not enforce INV-15 on non-Unix targets.
#[cfg(not(unix))]
#[derive(Debug)]
pub struct ExecutionLock;

#[cfg(not(unix))]
impl ExecutionLock {
    pub(crate) fn acquire(_lock_dir: &Path, _id: ExecutionId) -> Result<Self, DurableError> {
        Ok(Self)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_for_same_execution_fails() {
        let dir = tempfile::tempdir().unwrap();
        let id = ExecutionId::new();

        let _first = ExecutionLock::acquire(dir.path(), id).expect("first acquire succeeds");
        let err = ExecutionLock::acquire(dir.path(), id).expect_err("second acquire must fail");
        assert!(
            matches!(err, DurableError::ExecutionLocked { execution_id, .. } if execution_id == id),
            "expected ExecutionLocked for the same execution_id, got {err:?}"
        );
    }

    #[test]
    fn second_acquire_reports_holder_pid() {
        let dir = tempfile::tempdir().unwrap();
        let id = ExecutionId::new();

        let _first = ExecutionLock::acquire(dir.path(), id).expect("first acquire succeeds");
        let err = ExecutionLock::acquire(dir.path(), id).expect_err("second acquire must fail");
        let DurableError::ExecutionLocked { holder_pid, .. } = err else {
            panic!("expected ExecutionLocked, got {err:?}");
        };
        assert_eq!(
            holder_pid,
            std::process::id(),
            "holder_pid should report this test process's own pid (the only holder)"
        );
    }

    #[test]
    fn distinct_executions_do_not_contend() {
        let dir = tempfile::tempdir().unwrap();
        let a = ExecutionId::new();
        let b = ExecutionId::new();

        let _lock_a = ExecutionLock::acquire(dir.path(), a).expect("lock a succeeds");
        let _lock_b =
            ExecutionLock::acquire(dir.path(), b).expect("distinct execution_id does not block");
    }

    #[test]
    fn reacquire_after_drop_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let id = ExecutionId::new();

        {
            let _first = ExecutionLock::acquire(dir.path(), id).expect("first acquire succeeds");
        }
        let _second =
            ExecutionLock::acquire(dir.path(), id).expect("lock is released when guard drops");
    }

    #[test]
    fn lock_file_is_not_unlinked_on_drop() {
        // Regression test for critic finding S2: unlike `PidLockGuard`, the sentinel file must
        // survive release — only the flock itself signals ownership. Deleting it on drop reopens
        // the flock+unlink TOCTOU race under the higher-contention per-execution locking pattern.
        let dir = tempfile::tempdir().unwrap();
        let id = ExecutionId::new();
        let lock_path = dir.path().join(format!("{id}.lock"));

        {
            let _guard = ExecutionLock::acquire(dir.path(), id).expect("acquire succeeds");
            assert!(lock_path.exists(), "lock file must exist while held");
        }
        assert!(
            lock_path.exists(),
            "lock file must remain on disk after the guard drops (permanent sentinel)"
        );
    }
}
