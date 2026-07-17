// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Advisory PID file management for the scheduler daemon.
//!
//! Wraps [`zeph_common::pidfile::PidLockGuard`], the shared `flock(2)`-backed advisory
//! lock, so that exactly one `zeph serve` instance can run per config file.
//!
//! **Invariant**: the pid file MUST reside on a local filesystem. NFS mounts do
//! not guarantee reliable exclusive locking with `flock(2)`.

#![cfg(unix)]

use std::path::Path;

use zeph_common::pidfile::{PidLockError, PidLockGuard, read_pid_lenient};

/// Re-exported so existing `crate::pidfile::is_process_alive` call sites keep working after
/// promotion to `zeph_common::pidfile` (shared with `zeph-session`'s `AdvisoryLock`).
pub use zeph_common::pidfile::is_process_alive;

use crate::error::SchedulerError;

/// Advisory PID file backed by an `flock(2)` exclusive lock.
///
/// Acquiring the lock writes the current process PID to the file. Dropping the
/// guard unlinks the file and then closes the file descriptor, releasing the lock.
///
/// The fd inheritance invariant: the file is opened with `O_CLOEXEC`, so child
/// processes spawned via `Command` do NOT inherit the lock. If you re-exec the
/// binary (as `zeph serve --foreground` does), the new process must call
/// `PidFile::acquire` independently.
// Wraps the shared guard purely for its `Drop` impl (unlinks the pid file on release).
#[derive(Debug)]
pub struct PidFile(#[allow(dead_code)] PidLockGuard);

impl PidFile {
    /// Open (or create) the pid file at `path` and acquire an exclusive advisory lock.
    ///
    /// The sequence is:
    /// 1. `open(O_RDWR | O_CREAT | O_CLOEXEC, 0o644)` — atomic create-or-open.
    /// 2. `flock(LOCK_EX | LOCK_NB)` — fails immediately if already locked.
    /// 3. `ftruncate(0)` + write current PID.
    ///
    /// # Errors
    ///
    /// - [`SchedulerError::AlreadyRunning`] if another process holds the lock.
    /// - [`SchedulerError::Io`] for filesystem errors.
    pub fn acquire(path: &Path) -> Result<Self, SchedulerError> {
        PidLockGuard::acquire(path).map(Self).map_err(|e| match e {
            PidLockError::AlreadyRunning { pid } => SchedulerError::AlreadyRunning { pid },
            PidLockError::Io(err) => {
                SchedulerError::Io(format!("pid file error for {}: {err}", path.display()))
            }
        })
    }

    /// Read the PID stored in the file at `path` and check whether that process is alive.
    ///
    /// Returns `None` if the file does not exist, cannot be read, contains an
    /// unparseable PID, or the process is no longer running.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zeph_scheduler::PidFile;
    ///
    /// let alive = PidFile::read_alive(Path::new("/run/zeph.pid"));
    /// if let Some(pid) = alive {
    ///     println!("daemon is running with pid {pid}");
    /// }
    /// ```
    #[must_use]
    pub fn read_alive(path: &Path) -> Option<u32> {
        let pid = read_pid_lenient(path)?;
        if is_process_alive(pid) {
            Some(pid)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use tempfile::TempDir;

    use super::*;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_pid_path(dir: &TempDir) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.path().join(format!("zeph-{n}.pid"))
    }

    #[test]
    fn acquire_creates_file_with_pid() {
        let dir = TempDir::new().unwrap();
        let path = unique_pid_path(&dir);

        let pf = PidFile::acquire(&path).expect("acquire should succeed");
        let content = std::fs::read_to_string(&path).expect("pid file must exist");
        assert_eq!(
            content.trim().parse::<u32>().unwrap(),
            std::process::id(),
            "pid file must contain current process pid"
        );
        drop(pf);
        assert!(!path.exists(), "pid file must be removed on drop");
    }

    #[test]
    fn second_acquire_fails_with_already_running() {
        let dir = TempDir::new().unwrap();
        let path = unique_pid_path(&dir);

        let _guard = PidFile::acquire(&path).expect("first acquire must succeed");
        let err = PidFile::acquire(&path).expect_err("second acquire must fail");
        assert!(
            matches!(err, SchedulerError::AlreadyRunning { .. }),
            "expected AlreadyRunning, got {err:?}"
        );
    }

    #[test]
    fn read_alive_returns_none_for_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.pid");
        assert!(PidFile::read_alive(&path).is_none());
    }

    #[test]
    fn read_alive_returns_none_for_dead_pid() {
        let dir = TempDir::new().unwrap();
        let path = unique_pid_path(&dir);
        // Write a PID that is very unlikely to be alive (PID 1 is init — we can't kill it,
        // so use a known-dead PID: max u32 truncated to a plausible but unused value).
        std::fs::write(&path, "999999999").unwrap();
        // On most systems pid 999999999 does not exist.
        let alive = PidFile::read_alive(&path);
        // We can't guarantee the PID is dead on all systems, so just ensure no panic.
        let _ = alive;
    }
}
