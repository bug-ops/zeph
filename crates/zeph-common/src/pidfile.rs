// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared `flock(2)`-backed advisory pid-file guard.
//!
//! This is the single-instance-lock primitive used by both `zeph-core::daemon::PidGuard`
//! and `zeph-scheduler::pidfile::PidFile`. The lock is acquired with `LOCK_EX | LOCK_NB`
//! so a second invocation fails immediately rather than blocking, and the pid file is
//! unlinked when the guard is dropped.
//!
//! **Invariant**: the pid file MUST reside on a local filesystem. NFS mounts do not
//! guarantee reliable exclusive locking with `flock(2)`.
//!
//! Unix only — `flock(2)` has no portable equivalent, so callers on other platforms must
//! implement their own fallback (see `zeph-core::daemon::PidGuard`'s non-Unix branch).

#![cfg(unix)]

use std::path::{Path, PathBuf};

use rustix::fd::OwnedFd;
use rustix::fs::{FlockOperation, Mode, OFlags};

/// Error acquiring or maintaining a [`PidLockGuard`].
#[derive(Debug, thiserror::Error)]
pub enum PidLockError {
    /// Another process already holds the exclusive lock on the pid file.
    ///
    /// The inner `pid` is read back from the file's contents; it is `0` if the file could
    /// not be read or its contents could not be parsed as a PID.
    #[error("another process holds the lock (pid {pid})")]
    AlreadyRunning {
        /// PID of the process currently holding the lock, or `0` if unknown.
        pid: u32,
    },
    /// A filesystem error occurred while opening, locking, or writing the pid file.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Exclusive, `flock(2)`-backed guard on a pid file.
///
/// Acquiring the lock writes the current process PID to the file. Dropping the guard
/// unlinks the file and then closes the file descriptor, releasing the lock.
///
/// The fd inheritance invariant: the file is opened with `O_CLOEXEC`, so child processes
/// spawned via `Command` do NOT inherit the lock. If you re-exec the binary, the new
/// process must call [`PidLockGuard::acquire`] independently.
///
/// # Examples
///
/// ```
/// use zeph_common::pidfile::PidLockGuard;
///
/// let path = std::env::temp_dir().join(format!("zeph-pidfile-doctest-{}.pid", std::process::id()));
/// let guard = PidLockGuard::acquire(&path).expect("no other instance running");
/// assert_eq!(guard.path(), path.as_path());
/// drop(guard); // releases the lock and removes the pid file
/// assert!(!path.exists());
/// ```
#[derive(Debug)]
pub struct PidLockGuard {
    #[allow(dead_code)] // held for its Drop (closes fd, releases flock)
    fd: OwnedFd,
    path: PathBuf,
}

impl PidLockGuard {
    /// Open (or create) the pid file at `path` and acquire an exclusive advisory lock.
    ///
    /// The sequence is:
    /// 1. `open(O_RDWR | O_CREAT | O_CLOEXEC, 0o644)` — atomic create-or-open.
    /// 2. `flock(LOCK_EX | LOCK_NB)` — fails immediately if already locked.
    /// 3. `ftruncate(0)` + write current PID.
    ///
    /// # Errors
    ///
    /// - [`PidLockError::AlreadyRunning`] if another process holds the lock.
    /// - [`PidLockError::Io`] for filesystem errors.
    pub fn acquire(path: &Path) -> Result<Self, PidLockError> {
        // Create parent directory on-demand so first-run works out of the box.
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        let fd = rustix::fs::open(
            path,
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )
        .map_err(std::io::Error::from)?;

        // Try to acquire an exclusive non-blocking lock.
        rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            // EWOULDBLOCK means another process holds the lock.
            if e == rustix::io::Errno::WOULDBLOCK {
                let pid = read_pid_lenient(path).unwrap_or(0);
                PidLockError::AlreadyRunning { pid }
            } else {
                PidLockError::Io(e.into())
            }
        })?;

        // We hold the lock — truncate and write our PID.
        rustix::fs::ftruncate(&fd, 0).map_err(std::io::Error::from)?;
        rustix::io::write(&fd, std::process::id().to_string().as_bytes())
            .map_err(std::io::Error::from)?;

        Ok(Self {
            fd,
            path: path.to_owned(),
        })
    }

    /// Path to the locked pid file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PidLockGuard {
    fn drop(&mut self) {
        // Unlink first so a subsequent acquire attempt sees no stale file while we still
        // hold the lock. Then `fd` drops, closing the fd and releasing the flock.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Best-effort read of the PID stored in the file at `path`.
///
/// Returns `None` if the file does not exist, cannot be read, or its contents cannot be
/// parsed as a PID. Used to populate [`PidLockError::AlreadyRunning`] and by callers that
/// want to check pid-file contents without holding (or contending for) the lock.
///
/// # Examples
///
/// ```
/// use zeph_common::pidfile::read_pid_lenient;
///
/// let missing = std::env::temp_dir().join("zeph-pidfile-doctest-missing.pid");
/// assert_eq!(read_pid_lenient(&missing), None);
/// ```
#[must_use]
pub fn read_pid_lenient(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
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

        let guard = PidLockGuard::acquire(&path).expect("acquire should succeed");
        let content = std::fs::read_to_string(&path).expect("pid file must exist");
        assert_eq!(
            content.trim().parse::<u32>().unwrap(),
            std::process::id(),
            "pid file must contain current process pid"
        );
        drop(guard);
        assert!(!path.exists(), "pid file must be removed on drop");
    }

    #[test]
    fn second_acquire_fails_with_already_running() {
        let dir = TempDir::new().unwrap();
        let path = unique_pid_path(&dir);

        let _guard = PidLockGuard::acquire(&path).expect("first acquire must succeed");
        let err = PidLockGuard::acquire(&path).expect_err("second acquire must fail");
        assert!(
            matches!(err, PidLockError::AlreadyRunning { .. }),
            "expected AlreadyRunning, got {err:?}"
        );
    }

    #[test]
    fn read_pid_lenient_returns_none_for_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.pid");
        assert!(read_pid_lenient(&path).is_none());
    }

    #[test]
    fn read_pid_lenient_returns_none_for_invalid_content() {
        let dir = TempDir::new().unwrap();
        let path = unique_pid_path(&dir);
        std::fs::write(&path, "not_a_number").unwrap();
        assert!(read_pid_lenient(&path).is_none());
    }
}
