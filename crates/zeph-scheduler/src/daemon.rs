// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Daemon lifecycle for the scheduler: foreground runner and detach helper.
//!
//! # Fork safety invariant (CRITICAL-3)
//!
//! **No tokio runtime must be alive when the daemon is detached.** The binary
//! must call [`detach_and_run`] BEFORE starting the tokio runtime. The detach
//! strategy is re-exec: the binary respawns itself with `--foreground`, which
//! enters `run_foreground` inside a fresh runtime. This avoids all fork-after-
//! async-runtime hazards.
//!
//! # Log rotation
//!
//! Use `logrotate copytruncate` — the daemon appends to the log file and a
//! separate log-rotate step truncates the original. SIGHUP-based reopen is not
//! implemented in this MVP.

#![cfg(all(unix, feature = "daemon"))]

use std::path::PathBuf;
use std::process::Stdio;

use crate::error::SchedulerError;
use crate::pidfile::PidFile;
use crate::scheduler::Scheduler;

/// Configuration for the scheduler daemon process.
///
/// Typically constructed from `zeph_config::SchedulerDaemonConfig` by the binary.
///
/// # Example
///
/// ```no_run
/// use std::path::PathBuf;
/// use zeph_scheduler::DaemonConfig;
///
/// let cfg = DaemonConfig {
///     pid_file: PathBuf::from("/tmp/zeph.pid"),
///     log_file: PathBuf::from("/tmp/zeph.log"),
///     catch_up: true,
///     tick_secs: 60,
///     shutdown_grace_secs: 30,
/// };
/// ```
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Path to the advisory PID lock file. Must be on a local filesystem.
    pub pid_file: PathBuf,
    /// Path to the daemon log file (append-only; rotated externally).
    pub log_file: PathBuf,
    /// When `true`, fire overdue periodic tasks once on startup via
    /// [`Scheduler::catch_up_missed`].
    pub catch_up: bool,
    /// Tick interval in seconds (clamped to `5..=3600`).
    pub tick_secs: u64,
    /// Grace period in seconds after SIGTERM before the process exits.
    ///
    /// Clamped to 60 s internally by [`Scheduler::run_with_interval_and_grace`].
    /// Values above 60 are accepted without error but have no additional effect.
    pub shutdown_grace_secs: u64,
}

/// Status of the scheduler daemon, returned by [`daemon_status`].
///
/// The JSON shape is stable and used by both `zeph status --json` and the TUI
/// `/daemon status` command.
#[derive(Debug, serde::Serialize)]
pub struct DaemonStatus {
    /// Whether the daemon is currently running (pid file locked by a live process).
    pub running: bool,
    /// PID of the running daemon, or `None` if not running.
    pub pid: Option<u32>,
    /// Path to the pid file (as configured).
    pub pid_file: PathBuf,
    /// Path to the log file (as configured).
    pub log_file: PathBuf,
    /// Number of tasks currently registered in the store.
    pub task_count: usize,
    /// Most recent task runs (up to `n` entries, newest first).
    pub recent_runs: Vec<TaskRunSummary>,
}

/// Summary of a single task run, included in [`DaemonStatus`].
#[derive(Debug, serde::Serialize)]
pub struct TaskRunSummary {
    /// Task name.
    pub name: String,
    /// Execution mode: `"periodic"` or `"oneshot"`.
    pub mode: String,
    /// Last recorded run time (RFC 3339), or empty if never run.
    pub last_run: String,
    /// Scheduled next run time (RFC 3339), or empty if not applicable.
    pub next_run: String,
}

/// Run the scheduler in the current process (foreground / `--foreground` mode).
///
/// This is the entry point used by:
/// - `zeph serve --foreground` (systemd / launchd managed processes)
/// - The re-exec child spawned by [`detach_and_run`]
///
/// The function:
/// 1. Acquires the pid file (exclusive advisory lock).
/// 2. Optionally runs [`Scheduler::catch_up_missed`].
/// 3. Starts the tick loop via [`Scheduler::run_with_interval_and_grace`].
///
/// # Errors
///
/// Returns [`SchedulerError::AlreadyRunning`] if another daemon holds the pid
/// file lock, or other [`SchedulerError`] variants on store / handler failures.
pub async fn run_foreground(
    mut scheduler: Scheduler,
    cfg: &DaemonConfig,
) -> Result<(), SchedulerError> {
    let _span = tracing::info_span!(
        "scheduler.daemon.start",
        pid_file = %cfg.pid_file.display(),
        detached = false,
    )
    .entered();

    // Detach from the controlling terminal so the daemon survives terminal close.
    // setsid(2) only modifies session state and is safe to call after the tokio runtime
    // has started — it has no interaction with tokio threads. EPERM means we are already
    // a session leader (e.g. launched by systemd or launchd); ignore it.
    let _ = rustix::process::setsid();

    let _pidfile = PidFile::acquire(&cfg.pid_file)?;
    tracing::info!(
        pid = std::process::id(),
        pid_file = %cfg.pid_file.display(),
        "scheduler daemon started (foreground)"
    );

    scheduler.init().await?;

    if cfg.catch_up {
        scheduler.catch_up_missed().await?;
    }

    scheduler
        .run_with_interval_and_grace(cfg.tick_secs, cfg.shutdown_grace_secs)
        .await;

    tracing::info!("scheduler daemon stopped");
    Ok(())
}

/// Re-exec the current binary with `--foreground` to detach from the controlling
/// terminal without forking the tokio runtime.
///
/// # Fork safety (CRITICAL-3)
///
/// This function must be called **before** `tokio::main` or any runtime is started.
/// It uses [`std::process::Command`] (which calls `posix_spawn` or `fork+exec`
/// internally) so no async state is duplicated. The parent process exits after
/// the child is spawned successfully.
///
/// # PID file
///
/// The parent does NOT acquire the pid file. The spawned child enters
/// [`run_foreground`] which acquires the lock inside the new runtime.
///
/// # Log file
///
/// `stdout` and `stderr` of the child process are redirected to `cfg.log_file`
/// in append mode. `stdin` is connected to `/dev/null`.
///
/// # Arguments forwarded
///
/// All arguments from `extra_args` are forwarded to the child. The caller is
/// responsible for stripping any `--foreground` flags from `extra_args` and
/// appending the real `--foreground` flag.
///
/// # Errors
///
/// Returns [`SchedulerError::Detach`] if spawning the child process fails.
pub fn detach_and_run(cfg: &DaemonConfig, extra_args: &[&str]) -> Result<(), SchedulerError> {
    let _span = tracing::info_span!(
        "scheduler.daemon.start",
        pid_file = %cfg.pid_file.display(),
        detached = true,
    )
    .entered();

    // Create parent directories for the log file.
    if let Some(parent) = cfg.log_file.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| {
            SchedulerError::Detach(format!(
                "failed to create log directory {}: {e}",
                parent.display()
            ))
        })?;
    }

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cfg.log_file)
        .map_err(|e| {
            SchedulerError::Detach(format!(
                "failed to open log file {}: {e}",
                cfg.log_file.display()
            ))
        })?;

    let exe = std::env::current_exe()
        .map_err(|e| SchedulerError::Detach(format!("failed to resolve current exe: {e}")))?;

    // The child gets `stdout` and `stderr` on the log file; `stdin` is /dev/null.
    // NOTE: `log_file` is cloned for stderr; `try_clone` is infallible in practice here.
    let log_stderr = log_file
        .try_clone()
        .map_err(|e| SchedulerError::Detach(format!("failed to clone log fd: {e}")))?;

    let child = std::process::Command::new(&exe)
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_stderr)
        .spawn()
        .map_err(|e| {
            SchedulerError::Detach(format!("failed to spawn daemon child process: {e}"))
        })?;

    tracing::info!(
        child_pid = child.id(),
        exe = %exe.display(),
        log_file = %cfg.log_file.display(),
        "scheduler daemon detached"
    );

    // Parent exits — child continues as daemon.
    // We use std::process::exit(0) to bypass any Rust drop glue that might interfere with
    // the still-open log file fd in the child (which inherits a dup of the fd).
    std::process::exit(0);
}

/// Query the current daemon status without requiring the daemon to be running.
///
/// Reads the pid file for liveness and the job store for task counts and recent runs.
///
/// # Errors
///
/// Returns [`SchedulerError`] if the job store cannot be opened or queried.
pub async fn daemon_status(
    cfg: &DaemonConfig,
    store_url: &str,
    recent_n: usize,
) -> Result<DaemonStatus, SchedulerError> {
    let pid = PidFile::read_alive(&cfg.pid_file);
    let running = pid.is_some();

    let store = crate::store::JobStore::open(store_url).await?;
    store.init().await?;

    let jobs = store.list_jobs_full().await?;
    let task_count = jobs.len();

    let recent_runs: Vec<TaskRunSummary> = jobs
        .into_iter()
        .take(recent_n)
        .map(|j| TaskRunSummary {
            name: j.name,
            mode: j.task_mode,
            last_run: String::new(), // store does not expose last_run yet; extend in follow-up
            next_run: j.next_run,
        })
        .collect();

    Ok(DaemonStatus {
        running,
        pid,
        pid_file: cfg.pid_file.clone(),
        log_file: cfg.log_file.clone(),
        task_count,
        recent_runs,
    })
}

/// Send SIGTERM to the running daemon and wait up to `timeout_secs` for it to exit.
///
/// If the process has not exited after `timeout_secs`, SIGKILL is sent with a warning.
///
/// # Errors
///
/// Returns [`SchedulerError::Io`] if no daemon is running or the signal cannot be sent.
pub fn stop_daemon(cfg: &DaemonConfig, timeout_secs: u64) -> Result<(), SchedulerError> {
    let Some(pid) = PidFile::read_alive(&cfg.pid_file) else {
        return Err(SchedulerError::Io(format!(
            "no running daemon found (pid file: {})",
            cfg.pid_file.display()
        )));
    };

    // Send SIGTERM.
    let rustix_pid = rustix::process::Pid::from_raw(pid.cast_signed())
        .ok_or_else(|| SchedulerError::Io(format!("invalid pid {pid} in pid file")))?;

    rustix::process::kill_process(rustix_pid, rustix::process::Signal::TERM)
        .map_err(|e| SchedulerError::Io(format!("failed to send SIGTERM to pid {pid}: {e}")))?;

    tracing::info!(pid, "SIGTERM sent to scheduler daemon");

    // Poll for exit.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if !crate::pidfile::is_process_alive(pid) {
            tracing::info!(pid, "daemon stopped");
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
    }

    // Escalate to SIGKILL.
    tracing::warn!(
        pid,
        "daemon did not stop within {timeout_secs}s — sending SIGKILL"
    );
    let rustix_pid = rustix::process::Pid::from_raw(pid.cast_signed())
        .ok_or_else(|| SchedulerError::Io(format!("invalid pid {pid} in pid file")))?;
    rustix::process::kill_process(rustix_pid, rustix::process::Signal::KILL)
        .map_err(|e| SchedulerError::Io(format!("failed to send SIGKILL to pid {pid}: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::DaemonConfig;

    fn test_cfg() -> DaemonConfig {
        DaemonConfig {
            pid_file: PathBuf::from("/tmp/zeph-test.pid"),
            log_file: PathBuf::from("/tmp/zeph-test.log"),
            catch_up: true,
            tick_secs: 60,
            shutdown_grace_secs: 30,
        }
    }

    #[test]
    fn daemon_config_clone() {
        let cfg = test_cfg();
        let cfg2 = cfg.clone();
        assert_eq!(cfg.tick_secs, cfg2.tick_secs);
        assert_eq!(cfg.shutdown_grace_secs, cfg2.shutdown_grace_secs);
    }

    #[test]
    fn daemon_config_defaults_reasonable() {
        let cfg = test_cfg();
        assert!(cfg.tick_secs >= 5);
        assert!(cfg.shutdown_grace_secs >= 1);
    }
}
