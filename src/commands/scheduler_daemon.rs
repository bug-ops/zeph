// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! CLI handlers for `zeph serve`, `zeph stop`, and `zeph status`.
//!
//! These commands manage the scheduler daemon process. They are Unix-only
//! and require the `scheduler` feature.

#![cfg(all(unix, feature = "scheduler"))]

use anyhow::Context as _;

use crate::bootstrap::{load_config_or_default, resolve_config_path};

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
    let config = load_config_or_default(&config_file);
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
    let config = load_config_or_default(&config_file);
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
    let config = load_config_or_default(&config_file);
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
        handler_timeout_secs: sched.handler_timeout_secs,
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

    let sched_cancel = tokio_util::sync::CancellationToken::new();
    let sched_supervisor = zeph_common::TaskSupervisor::new(sched_cancel.clone());

    // Gracefully shut down on SIGTERM/SIGINT.
    {
        let signal_fut = async move {
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
        };
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(signal_fut)));
        sched_supervisor.spawn(zeph_common::TaskDescriptor {
            name: "sched_daemon_signal",
            restart: zeph_common::RestartPolicy::RunOnce,
            factory: move || {
                let f = cell.lock().take();
                async move {
                    if let Some(f) = f {
                        f.await;
                    }
                }
            },
        });
    }

    let (mut scheduler, ctrl_tx) = zeph_scheduler::Scheduler::new(store, shutdown_rx);
    scheduler = scheduler.with_reentry_defense(
        config.scheduler.security.enabled,
        config.scheduler.security.injection_pattern_check,
        config.scheduler.security.attenuate_after_external_read,
    );

    if let Some(adapter) = build_durable_adapter(config, &sched_supervisor).await? {
        scheduler = scheduler.with_durable(adapter);
    }

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

    let result = zeph_scheduler::run_foreground(scheduler, &daemon_cfg)
        .await
        .context("scheduler daemon exited with error");
    sched_cancel.cancel();
    sched_supervisor
        .shutdown_all(std::time::Duration::from_secs(5))
        .await;
    result
}

/// Open the scheduler's durable backend, attach the write-path AEAD cipher when
/// `[durable] encrypt_payload = true` and the control-entry HMAC key when this deployment is a
/// declared/detected shared database (INV-8), then spawn its journal-writer task under
/// `sched_supervisor`.
///
/// Returns `Ok(None)` when durable execution is disabled for the scheduler
/// (`[durable] enabled = false` or `[durable] scheduler = false`), so the caller runs without
/// durable execution unchanged.
///
/// # Errors
///
/// Returns an error if the durable backend cannot be opened or initialized, if the AEAD cipher
/// cannot be resolved when `encrypt_payload = true`, or if the control-entry HMAC key cannot be
/// resolved for a shared database — every case fails closed instead of silently running with a
/// weaker security posture.
async fn build_durable_adapter(
    config: &zeph_core::config::Config,
    sched_supervisor: &zeph_common::TaskSupervisor,
) -> anyhow::Result<Option<zeph_scheduler::durable::SchedulerDurableAdapter>> {
    if !(config.durable.enabled && config.durable.scheduler) {
        return Ok(None);
    }
    // Resolve the write-path cipher (which evaluates the INV-8 encryption_gate, #5996) and the
    // control-entry HMAC key (#6043/#6044) before opening the backend, so a forbidden config
    // fails closed without creating a stray empty journal file on disk first.
    let cipher = crate::commands::durable::load_write_cipher(config)?;
    let hmac_key = crate::commands::durable::load_write_hmac_key(config)?;
    let durable_url = crate::commands::durable::resolve_durable_db_url(config);
    let local = zeph_durable::LocalBackend::open(&durable_url, config.durable.max_payload_bytes)
        .await
        .context("failed to open scheduler durable backend")?;
    local
        .init()
        .await
        .context("failed to init scheduler durable schema")?;
    let local = if let Some(cipher) = cipher {
        local.with_cipher(cipher)
    } else {
        local
    };
    let local = if let Some(key) = hmac_key {
        local.with_hmac_key(key)
    } else {
        local
    };
    let local = std::sync::Arc::new(local);
    let backend = std::sync::Arc::new(zeph_durable::DurableBackendEnum::Local(local.clone()));
    let durable_cfg = std::sync::Arc::new(config.durable.clone());
    let (writer_actor, writer_handle) = zeph_durable::JournalWriter::new(local, &durable_cfg);
    {
        let writer_fut = writer_actor.run();
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(writer_fut)));
        sched_supervisor.spawn(zeph_common::TaskDescriptor {
            name: "journal_writer",
            restart: zeph_common::RestartPolicy::RunOnce,
            factory: move || {
                let f = cell.lock().take();
                async move {
                    if let Some(f) = f {
                        f.await;
                    }
                }
            },
        });
    }
    Ok(Some(zeph_scheduler::durable::SchedulerDurableAdapter::new(
        backend,
        writer_handle,
        durable_cfg,
    )))
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
            let last_run = if run.last_run.is_empty() {
                "never"
            } else {
                run.last_run.as_str()
            };
            println!(
                "  {:<24} last: {:<25} next: {}",
                run.name, last_run, run.next_run
            );
        }
    }
}
