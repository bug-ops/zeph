// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Periodic system-metrics background task.
//!
//! Emits RSS, CPU%, thread count (Linux only), and file-descriptor count (Linux only)
//! as `tracing` events at a configurable interval. Consumers can collect these via
//! a file layer, `MetricsBridge`, or any other `tracing-subscriber` layer.

use sysinfo::{ProcessesToUpdate, System};
use tokio::task::JoinHandle;

/// Spawn a background task that periodically emits system metrics as tracing events.
///
/// The task samples the current process's RSS, CPU%, thread count, and file-descriptor
/// count at the configured interval and emits them as:
///
/// ```text
/// tracing::info!(target: "system.metrics", rss_bytes, cpu_percent, thread_count, fd_count);
/// ```
///
/// Some metrics are platform-specific:
/// - `fd_count`: Linux only (reads `/proc/{pid}/fd`). Returns 0 on other platforms.
/// - `thread_count`: Linux only (via `Process::tasks()`). Returns 0 on other platforms.
///
/// # Arguments
///
/// * `interval_secs` — sampling interval in seconds. Clamped to minimum 1. `0` disables the
///   task entirely.
/// * `shutdown_rx` — watch channel receiver; the task exits when the value changes.
///
/// # Returns
///
/// `Some(JoinHandle)` for the spawned task, or `None` when `interval_secs == 0`.
///
/// # Examples
///
/// ```no_run
/// # async fn example() {
/// let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
/// let _handle = zeph_core::system_metrics::spawn_system_metrics_task(5, shutdown_rx);
/// // ...
/// let _ = shutdown_tx.send(true);
/// # }
/// ```
#[must_use]
pub fn spawn_system_metrics_task(
    interval_secs: u64,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Option<JoinHandle<()>> {
    if interval_secs == 0 {
        return None;
    }
    let interval = std::time::Duration::from_secs(interval_secs.max(1));

    Some(tokio::spawn(async move {
        let mut sys = System::new();
        let pid = sysinfo::get_current_pid().ok();
        let mut shutdown = shutdown_rx;
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }

            let (rss_bytes, cpu_percent, thread_count, fd_count) = match pid {
                Some(pid) => {
                    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
                    match sys.process(pid) {
                        Some(proc_info) => {
                            let rss = proc_info.memory();
                            let cpu = proc_info.cpu_usage();
                            let threads = get_thread_count(proc_info);
                            let fds = get_fd_count(pid);
                            (rss, cpu, threads, fds)
                        }
                        None => (0, 0.0, 0, 0),
                    }
                }
                None => (0, 0.0, 0, 0),
            };

            tracing::info!(
                target: "system.metrics",
                rss_bytes,
                cpu_percent,
                thread_count,
                fd_count,
            );
        }

        tracing::debug!("system metrics task shutting down");
    }))
}

/// Get the thread count for the current process.
///
/// Uses `Process::tasks()` on Linux, which reads `/proc/{pid}/task`.
/// Returns 0 on other platforms where this information is unavailable.
#[cfg(target_os = "linux")]
fn get_thread_count(proc_info: &sysinfo::Process) -> u64 {
    proc_info.tasks().map_or(0, |tasks| tasks.len() as u64)
}

#[cfg(not(target_os = "linux"))]
fn get_thread_count(_proc_info: &sysinfo::Process) -> u64 {
    0
}

/// Get the file-descriptor count for the current process.
///
/// Counts entries in `/proc/{pid}/fd` on Linux.
/// Returns 0 on other platforms where `sysinfo::Process::fd()` is no longer available
/// (removed in sysinfo 0.33+).
#[cfg(target_os = "linux")]
fn get_fd_count(pid: sysinfo::Pid) -> u64 {
    let fd_path = format!("/proc/{}/fd", pid.as_u32());
    std::fs::read_dir(fd_path)
        .map(|entries| entries.count() as u64)
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn get_fd_count(_pid: sysinfo::Pid) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `interval_secs = 0` must return `None` without spawning any task.
    #[test]
    fn interval_zero_returns_none() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        assert!(
            spawn_system_metrics_task(0, rx).is_none(),
            "interval_secs=0 must return None"
        );
    }

    /// `interval_secs > 0` must return `Some(JoinHandle)`.
    #[tokio::test]
    async fn interval_nonzero_returns_some_handle() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let handle = spawn_system_metrics_task(1, rx);
        assert!(
            handle.is_some(),
            "interval_secs=1 must return Some(JoinHandle)"
        );
        if let Some(h) = handle {
            h.abort();
        }
    }

    /// Sending `true` on the shutdown channel terminates the task cleanly.
    #[tokio::test]
    async fn shutdown_via_watch_channel_terminates_task() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = spawn_system_metrics_task(60, rx).expect("interval=60 must return Some");

        // Signal shutdown and wait for the task to finish.
        tx.send(true).expect("shutdown send must succeed");
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(
            result.is_ok(),
            "task must exit within 5s after shutdown signal"
        );
        assert!(result.unwrap().is_ok(), "task must not panic on shutdown");
    }
}
