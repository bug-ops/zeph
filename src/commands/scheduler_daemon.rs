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
/// `[durable] encrypt_payload = true`, the control-entry HMAC key when this deployment is a
/// declared/detected shared database (INV-8), and the high-water-mark key unconditionally
/// (FR-009, addendum to #6451 — previously never attached on this channel at all), then spawn its
/// journal-writer task under `sched_supervisor`.
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
    // Resolve the write-path cipher (which evaluates the INV-8 encryption_gate, #5996), the
    // control-entry HMAC key (#6043/#6044), and the high-water-mark key (addendum to #6451)
    // before opening the backend, so a forbidden config fails closed without creating a stray
    // empty journal file on disk first.
    let cipher = crate::commands::durable::load_write_cipher(config)?;
    let hmac_keys = crate::commands::durable::load_write_hmac_key(config)?;
    let hwm_keys = crate::commands::durable::load_write_hwm_key(config)?;
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
    let local = if let Some(key) = hmac_keys.current {
        local.with_hmac_key(key)
    } else {
        local
    };
    let local = if let Some(key) = hmac_keys.previous {
        local.with_previous_hmac_key(key)
    } else {
        local
    };
    let local = if let Some(slot) = hwm_keys.current {
        local.with_hwm_key(slot.epoch, slot.key)
    } else {
        local
    };
    let local = if let Some(slot) = hwm_keys.previous {
        local.with_previous_hwm_key(slot.epoch, slot.key)
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
    {
        let retention_backend = std::sync::Arc::clone(&backend);
        let retention_policy = durable_cfg.retention.clone();
        sched_supervisor.spawn(zeph_common::TaskDescriptor {
            name: "durable.retention_sweep",
            restart: zeph_common::RestartPolicy::Restart {
                max: 5,
                base_delay: std::time::Duration::from_secs(5),
            },
            factory: move || {
                zeph_durable::DurableRetentionService::new(
                    std::sync::Arc::clone(&retention_backend),
                    retention_policy.clone(),
                )
                .run()
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

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_durable::Journal as _;

    /// #6264: `build_durable_adapter` must spawn the retention sweep (not just the journal
    /// writer) onto `sched_supervisor`, mirroring the P1/P2 coverage in
    /// `crates/zeph-core/src/agent/durable_bootstrap.rs`. `encrypt_payload = false` avoids a
    /// real vault dependency in this test, same pattern as
    /// `open_backend_reveal_succeeds_without_key_when_encrypt_payload_disabled` in
    /// `src/commands/durable.rs`.
    #[tokio::test]
    async fn spawns_retention_sweep_alongside_journal_writer() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = zeph_core::config::Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        config.durable.enabled = true;
        config.durable.scheduler = true;
        config.durable.encrypt_payload = false;

        let cancel = tokio_util::sync::CancellationToken::new();
        let sched_supervisor = zeph_common::TaskSupervisor::new(cancel);

        let adapter = build_durable_adapter(&config, &sched_supervisor)
            .await
            .expect("build_durable_adapter must succeed with a local, unencrypted backend");
        assert!(
            adapter.is_some(),
            "durable.enabled && durable.scheduler must produce an adapter"
        );

        let names: Vec<String> = sched_supervisor
            .snapshot()
            .iter()
            .map(|s| s.name.to_string())
            .collect();
        assert!(
            names.contains(&"journal_writer".to_owned()),
            "expected journal_writer among supervised tasks, got {names:?}"
        );
        assert!(
            names.contains(&"durable.retention_sweep".to_owned()),
            "expected durable.retention_sweep among supervised tasks, got {names:?}"
        );
    }

    /// The retention sweep must not spawn at all when the P3 adapter itself is disabled — it
    /// rides the same `durable.enabled && durable.scheduler` gate as backend construction.
    #[tokio::test]
    async fn does_not_spawn_when_scheduler_adapter_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = zeph_core::config::Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        config.durable.enabled = true;
        config.durable.scheduler = false;

        let cancel = tokio_util::sync::CancellationToken::new();
        let sched_supervisor = zeph_common::TaskSupervisor::new(cancel);

        let adapter = build_durable_adapter(&config, &sched_supervisor)
            .await
            .expect("build_durable_adapter must succeed (returns None, not an error)");
        assert!(adapter.is_none());
        assert!(sched_supervisor.snapshot().is_empty());
    }

    /// #6451 regression (critic finding 2): the scheduler-daemon read channel is one of the three
    /// runtime paths that must stay able to verify a pre-rotation `EffectIntent` control entry
    /// through an open rotation window. `build_durable_adapter` resolves both HMAC keys via
    /// `load_write_hmac_key` and attaches them (lines above, `hmac_keys.current` /
    /// `hmac_keys.previous`) exactly like this test's own re-derivation, so asserting both that
    /// adapter construction succeeds and that a backend opened with the same resolved keys can
    /// read a payload-less control entry stamped under the previous key is a faithful regression
    /// for this channel's wiring — including the crash-orphan shape (`EffectIntent` never carries
    /// a payload, so the AEAD blob-scan alone could never have caught a missed wiring here).
    #[allow(unsafe_code)]
    #[tokio::test]
    #[serial_test::serial]
    async fn scheduler_daemon_reads_previous_key_control_entry_through_rotation_window() {
        let vault_dir = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", vault_dir.path());
        }

        let vault_root = zeph_core::vault::default_vault_dir();
        zeph_core::vault::AgeVaultProvider::init_vault(&vault_root).unwrap();
        let mut provider = zeph_core::vault::AgeVaultProvider::load(
            &vault_root.join("vault-key.txt"),
            &vault_root.join("secrets.age"),
        )
        .unwrap();
        let old_key_b64 = zeph_core::durable::generate_durable_key_b64();
        let new_key_b64 = zeph_core::durable::generate_durable_key_b64();
        provider
            .set_secret_mut(
                "ZEPH_DURABLE_KEY_PREVIOUS".to_owned(),
                old_key_b64.clone(),
                false,
            )
            .unwrap();
        provider
            .set_secret_mut("ZEPH_DURABLE_KEY".to_owned(), new_key_b64, false)
            .unwrap();
        provider.save().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut config = zeph_core::config::Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        config.durable.enabled = true;
        config.durable.scheduler = true;
        config.durable.shared_db = true;
        config.durable.key_id = 1;
        config.durable.previous_key_id = Some(0);

        // Simulate a pre-rotation control entry: write it directly under the old (now-previous)
        // HMAC key, before the daemon ever opens the backend.
        let old_hmac_key = zeph_core::durable::derive_control_hmac_key_b64(&old_key_b64).unwrap();
        let durable_url = crate::commands::durable::resolve_durable_db_url(&config);
        let exec = zeph_durable::ExecutionId::new();
        {
            let pre_rotation_writer =
                zeph_durable::LocalBackend::open(&durable_url, config.durable.max_payload_bytes)
                    .await
                    .unwrap()
                    .with_hmac_key(old_hmac_key);
            pre_rotation_writer.init().await.unwrap();
            pre_rotation_writer
                .open_execution(exec, zeph_durable::ExecutionKind::AgentTurn)
                .await
                .unwrap();
            let step_id = zeph_durable::StepId::new(0);
            pre_rotation_writer
                .append(zeph_durable::JournalEntry {
                    seq: None,
                    execution_id: exec,
                    kind: zeph_durable::ExecutionKind::AgentTurn,
                    step_id,
                    entry: zeph_durable::EntryKind::EffectIntent {
                        idempotency_key: zeph_durable::IdempotencyKey::derive(
                            exec,
                            step_id,
                            b"transfer",
                        ),
                        effect: zeph_durable::EffectClass::ExactlyOnceGuarded,
                        hmac: None,
                    },
                    created_at_ms: 0,
                })
                .await
                .unwrap();
        }

        let cancel = tokio_util::sync::CancellationToken::new();
        let sched_supervisor = zeph_common::TaskSupervisor::new(cancel);
        let adapter = build_durable_adapter(&config, &sched_supervisor).await;
        assert!(
            adapter.is_ok() && adapter.unwrap().is_some(),
            "build_durable_adapter must succeed with a consistent rotation window declared"
        );

        // Re-derive the same keys `build_durable_adapter` resolved, and confirm the pre-rotation
        // entry is still readable through the window (the actual regression assertion) — still
        // under the guard's vault dir, before it is restored below.
        let hmac_keys = crate::commands::durable::load_write_hmac_key(&config).unwrap();
        let reader =
            zeph_durable::LocalBackend::open(&durable_url, config.durable.max_payload_bytes)
                .await
                .unwrap()
                .with_hmac_key(hmac_keys.current.unwrap())
                .with_previous_hmac_key(hmac_keys.previous.unwrap());
        let read_result = reader.read_execution(exec).await;

        unsafe {
            match &prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert!(
            read_result.is_ok(),
            "the scheduler-daemon read channel must verify a pre-rotation EffectIntent control \
             entry through the rotation window"
        );
    }

    /// Addendum to #6451 (tester-2 finding): mirrors
    /// `scheduler_daemon_reads_previous_key_control_entry_through_rotation_window` above, but for
    /// the high-water-mark path instead of the control-HMAC path. `build_durable_adapter`
    /// previously never attached an HWM key at all on this channel (confirmed absent before this
    /// addendum — unlike the control-HMAC key, the HWM key is meant to be attached
    /// unconditionally per FR-009, so this test does not need `shared_db = true`), so this proves
    /// both that it now attaches the current epoch unconditionally and the previous epoch while
    /// the window is open, by resuming a pre-rotation execution with a committed `StepResult`
    /// through the daemon's own resolved keys.
    #[allow(unsafe_code)]
    #[tokio::test]
    #[serial_test::serial]
    async fn scheduler_daemon_resumes_previous_epoch_high_water_mark_through_rotation_window() {
        let vault_dir = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", vault_dir.path());
        }

        let vault_root = zeph_core::vault::default_vault_dir();
        zeph_core::vault::AgeVaultProvider::init_vault(&vault_root).unwrap();
        let mut provider = zeph_core::vault::AgeVaultProvider::load(
            &vault_root.join("vault-key.txt"),
            &vault_root.join("secrets.age"),
        )
        .unwrap();
        let old_key_b64 = zeph_core::durable::generate_durable_key_b64();
        let new_key_b64 = zeph_core::durable::generate_durable_key_b64();
        provider
            .set_secret_mut(
                "ZEPH_DURABLE_KEY_PREVIOUS".to_owned(),
                old_key_b64.clone(),
                false,
            )
            .unwrap();
        provider
            .set_secret_mut("ZEPH_DURABLE_KEY".to_owned(), new_key_b64, false)
            .unwrap();
        provider.save().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut config = zeph_core::config::Config::default();
        config.memory.sqlite_path = dir.path().join("zeph.db").to_string_lossy().into_owned();
        config.durable.enabled = true;
        config.durable.scheduler = true;
        config.durable.key_id = 1;
        config.durable.previous_key_id = Some(0);

        // Commit a StepResult under the OLD (now-previous) HWM key, before the daemon ever opens
        // the backend -- simulating an execution that committed a result pre-rotation.
        let old_hwm_key = zeph_core::durable::derive_hwm_key_b64(&old_key_b64).unwrap();
        let durable_url = crate::commands::durable::resolve_durable_db_url(&config);
        let exec = zeph_durable::ExecutionId::new();
        {
            let pre_rotation_writer =
                zeph_durable::LocalBackend::open(&durable_url, config.durable.max_payload_bytes)
                    .await
                    .unwrap()
                    .with_hwm_key(0, old_hwm_key);
            pre_rotation_writer.init().await.unwrap();
            pre_rotation_writer
                .open_execution(exec, zeph_durable::ExecutionKind::AgentTurn)
                .await
                .unwrap();
            let step_id = zeph_durable::StepId::new(0);
            pre_rotation_writer
                .append(zeph_durable::JournalEntry {
                    seq: None,
                    execution_id: exec,
                    kind: zeph_durable::ExecutionKind::AgentTurn,
                    step_id,
                    entry: zeph_durable::EntryKind::StepResult {
                        idempotency_key: zeph_durable::IdempotencyKey::derive(
                            exec,
                            step_id,
                            b"tool:test",
                        ),
                        payload: bytes::Bytes::copy_from_slice(b"pre-rotation result"),
                        effect: zeph_durable::EffectClass::Idempotent,
                        payload_version: 1,
                    },
                    created_at_ms: 0,
                })
                .await
                .unwrap();
        }

        let cancel = tokio_util::sync::CancellationToken::new();
        let sched_supervisor = zeph_common::TaskSupervisor::new(cancel);
        let adapter = build_durable_adapter(&config, &sched_supervisor).await;
        assert!(
            adapter.is_ok() && adapter.unwrap().is_some(),
            "build_durable_adapter must succeed with a consistent rotation window declared"
        );

        // Re-derive the same HWM keys `build_durable_adapter` resolved, and confirm the
        // pre-rotation execution still resumes cleanly through the window (the actual regression
        // assertion) -- still under the guard's vault dir, before it is restored below.
        let hwm_keys = crate::commands::durable::load_write_hwm_key(&config).unwrap();
        let current = hwm_keys.current.expect("current HWM slot must resolve");
        let previous = hwm_keys
            .previous
            .expect("previous HWM slot must resolve while the window is open");
        let reader =
            zeph_durable::LocalBackend::open(&durable_url, config.durable.max_payload_bytes)
                .await
                .unwrap()
                .with_hwm_key(current.epoch, current.key)
                .with_previous_hwm_key(previous.epoch, previous.key);
        let resume_result = reader
            .open_execution(exec, zeph_durable::ExecutionKind::AgentTurn)
            .await;

        unsafe {
            match &prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert!(
            resume_result.is_ok(),
            "the scheduler-daemon read channel must resume a pre-rotation execution's \
             high-water-mark through the rotation window: {resume_result:?}"
        );
    }
}
