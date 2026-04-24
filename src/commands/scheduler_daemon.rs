// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! CLI handlers for `zeph serve`, `zeph stop`, and `zeph status`.
//!
//! These commands manage the scheduler daemon process. They are Unix-only
//! and require the `scheduler` feature.

#![cfg(all(unix, feature = "scheduler"))]

use anyhow::Context as _;

use crate::bootstrap::resolve_config_path;

/// Handle `zeph serve [--foreground] [--no-catch-up]`.
///
/// Starts the scheduler daemon. Without `--foreground`, re-execs with
/// `--foreground` to detach without forking a live tokio runtime.
pub(crate) async fn handle_serve(
    config_path: Option<&std::path::Path>,
    foreground: bool,
    catch_up: bool,
) -> anyhow::Result<()> {
    let config_file = resolve_config_path(config_path);
    let config = zeph_core::config::Config::load(&config_file).unwrap_or_default();
    let daemon_cfg = build_daemon_config(&config);

    if foreground {
        run_foreground(daemon_cfg, &config).await
    } else {
        // Build args for the re-exec child. Pass --config so the child resolves
        // the same config file, then `serve --foreground` with catch-up flag.
        let config_str = config_file.to_string_lossy();
        let mut extra: Vec<&str> = vec!["--config", &config_str, "serve", "--foreground"];
        if !catch_up {
            extra.push("--no-catch-up");
        }
        zeph_scheduler::detach_and_run(&daemon_cfg, &extra)
            .context("failed to detach scheduler daemon")
    }
}

/// Handle `zeph stop [--timeout-secs N]`.
pub(crate) fn handle_stop(
    config_path: Option<&std::path::Path>,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let config_file = resolve_config_path(config_path);
    let config = zeph_core::config::Config::load(&config_file).unwrap_or_default();
    let daemon_cfg = build_daemon_config(&config);

    zeph_scheduler::stop_daemon(&daemon_cfg, timeout_secs)
        .context("failed to stop scheduler daemon")
}

/// Handle `zeph status [--json] [-n N]`.
pub(crate) async fn handle_status(
    config_path: Option<&std::path::Path>,
    json: bool,
    n: usize,
) -> anyhow::Result<()> {
    let config_file = resolve_config_path(config_path);
    let config = zeph_core::config::Config::load(&config_file).unwrap_or_default();
    let daemon_cfg = build_daemon_config(&config);
    let db_url = crate::db_url::resolve_db_url(&config);

    let status = zeph_scheduler::daemon_status(&daemon_cfg, db_url, n)
        .await
        .context("failed to read daemon status")?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status).context("failed to serialize daemon status")?
        );
    } else {
        print_status_human(&status);
    }
    Ok(())
}

fn build_daemon_config(config: &zeph_core::config::Config) -> zeph_scheduler::DaemonConfig {
    let sched = &config.scheduler.daemon;
    zeph_scheduler::DaemonConfig {
        pid_file: std::path::PathBuf::from(&sched.pid_file),
        log_file: std::path::PathBuf::from(&sched.log_file),
        catch_up: sched.catch_up,
        tick_secs: sched.tick_secs,
        shutdown_grace_secs: sched.shutdown_grace_secs,
    }
}

async fn run_foreground(
    daemon_cfg: zeph_scheduler::DaemonConfig,
    config: &zeph_core::config::Config,
) -> anyhow::Result<()> {
    let db_url = crate::db_url::resolve_db_url(config);
    let store = zeph_scheduler::JobStore::open(db_url)
        .await
        .context("failed to open scheduler store")?;
    store
        .init()
        .await
        .context("failed to init scheduler store")?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Gracefully shut down on SIGTERM/SIGINT.
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
        let mut sigint =
            signal(SignalKind::interrupt()).expect("failed to register SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sigint.recv() => tracing::info!("received SIGINT"),
        }
        let _ = shutdown_tx.send(true);
    });

    let (mut scheduler, ctrl_tx) = zeph_scheduler::Scheduler::new(store, shutdown_rx);

    // Register built-in handlers available without a live agent session.
    // `UpdateCheckHandler` is self-contained (HTTP only); other handlers that
    // require the agent loop (CustomTaskHandler, ExperimentTaskHandler) are not
    // registered in daemon mode — their tasks will be skipped with a warning.
    if config.agent.auto_update_check {
        let (update_tx, _update_rx) = tokio::sync::mpsc::channel(4);
        let handler = zeph_scheduler::UpdateCheckHandler::new(env!("CARGO_PKG_VERSION"), update_tx);
        scheduler.register_handler(&zeph_scheduler::TaskKind::UpdateCheck, Box::new(handler));
    }

    // Load periodic/one-shot tasks declared in [scheduler.tasks].
    crate::scheduler::load_config_tasks(&config.scheduler.tasks, &ctrl_tx);

    zeph_scheduler::run_foreground(scheduler, &daemon_cfg)
        .await
        .context("scheduler daemon exited with error")
}

fn print_status_human(status: &zeph_scheduler::DaemonStatus) {
    let running = if status.running {
        "running"
    } else {
        "not running"
    };
    let pid_str = status
        .pid
        .map(|p| format!(" (pid {p})"))
        .unwrap_or_default();

    println!("daemon:    {running}{pid_str}");
    println!("pid_file:  {}", status.pid_file.display());
    println!("log_file:  {}", status.log_file.display());
    println!("tasks:     {}", status.task_count);

    if !status.recent_runs.is_empty() {
        println!("last runs:");
        for run in &status.recent_runs {
            println!("  {:<24} next: {}", run.name, run.next_run);
        }
    }
}
