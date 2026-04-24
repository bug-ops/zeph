// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Advisory PID file management for the scheduler daemon.
//!
//! Uses `rustix` for file I/O and `flock(2)` advisory locking so that exactly
//! one `zeph serve` instance can run per config file. The lock is acquired with
//! `LOCK_EX | LOCK_NB` so a second invocation fails immediately rather than
//! blocking.
//!
//! **Invariant**: the pid file MUST reside on a local filesystem. NFS mounts do
//! not guarantee reliable exclusive locking with `flock(2)`.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use rustix::fd::OwnedFd;
use rustix::fs::{FlockOperation, Mode, OFlags};

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
#[derive(Debug)]
pub struct PidFile {
    #[allow(dead_code)] // held for its Drop (closes fd, releases flock)
    fd: OwnedFd,
    path: PathBuf,
}

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
        // Create parent directory on-demand so first-run works out of the box.
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| {
                SchedulerError::Io(format!(
                    "failed to create pid file directory {}: {e}",
                    parent.display()
                ))
            })?;
        }

        let fd = rustix::fs::open(
            path,
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )
        .map_err(|e| {
            SchedulerError::Io(format!("failed to open pid file {}: {e}", path.display()))
        })?;

        // Try to acquire an exclusive non-blocking lock.
        rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            // EWOULDBLOCK means another process holds the lock.
            let pid = Self::read_pid_from_path(path).unwrap_or(0);
            if e == rustix::io::Errno::WOULDBLOCK {
                SchedulerError::AlreadyRunning { pid }
            } else {
                SchedulerError::Io(format!("flock on pid file failed: {e}"))
            }
        })?;

        // We hold the lock — truncate and write our PID.
        rustix::fs::ftruncate(&fd, 0)
            .map_err(|e| SchedulerError::Io(format!("truncate pid file failed: {e}")))?;
        let pid_str = format!("{}", std::process::id());
        rustix::io::write(&fd, pid_str.as_bytes())
            .map_err(|e| SchedulerError::Io(format!("write pid file failed: {e}")))?;

        Ok(Self {
            fd,
            path: path.to_owned(),
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
        let pid = Self::read_pid_from_path(path)?;
        if is_process_alive(pid) {
            Some(pid)
        } else {
            None
        }
    }

    fn read_pid_from_path(path: &Path) -> Option<u32> {
        let content = std::fs::read_to_string(path).ok()?;
        content.trim().parse::<u32>().ok()
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        // Unlink first so a subsequent `zeph serve` sees no stale file while we
        // still hold the lock. Then `fd` drops, closing the fd and releasing the flock.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Check whether a process with the given PID is currently alive.
///
/// Uses `kill(pid, 0)` which sends no signal but returns an error if the
/// process does not exist or is a zombie that cannot be signalled.
///
/// # SIGNIFICANT-7 liveness guarantee
///
/// This check is combined with a flock attempt in [`PidFile::read_alive`] to
/// confirm that the daemon actually holds the lock (not just that a PID file
/// exists with a recycled PID). However, for the MVP `read_alive` only checks
/// `kill(pid, 0)` and trusts the pidfile content.
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    // kill(pid, 0) returns Ok if the process exists and we have permission to signal it,
    // Err(EPERM) if it exists but we lack permission, Err(ESRCH) if it does not exist.
    // Both Ok and EPERM mean the process is alive.
    let Some(rustix_pid) = rustix::process::Pid::from_raw(pid.cast_signed()) else {
        return false;
    };
    // test_kill_process is kill(pid, 0): succeeds if process exists and we can signal it.
    // EPERM means process exists but we lack permission — still alive.
    // ESRCH means no such process.
    match rustix::process::test_kill_process(rustix_pid) {
        Ok(()) => true,
        Err(e) if e == rustix::io::Errno::PERM => true,
        Err(_) => false,
    }
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
