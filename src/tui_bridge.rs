// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "tui")]
use std::time::Duration;

#[cfg(feature = "tui")]
use crate::bootstrap::warmup_provider;
#[cfg(feature = "tui")]
use crate::channel::TuiHandle;
#[cfg(feature = "tui")]
use zeph_core::channel::Channel;
#[cfg(feature = "tui")]
use zeph_llm::any::AnyProvider;

#[cfg(feature = "tui")]
pub(crate) struct TuiRunParams<'a> {
    pub(crate) tui_handle: TuiHandle,
    pub(crate) config: &'a zeph_core::config::Config,
    pub(crate) status_rx: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    pub(crate) tool_rx: Option<tokio::sync::mpsc::Receiver<zeph_tools::ToolEvent>>,
    pub(crate) metrics_rx:
        Option<tokio::sync::watch::Receiver<zeph_core::metrics::MetricsSnapshot>>,
    pub(crate) warmup_provider: AnyProvider,
    pub(crate) index_progress_rx: Option<tokio::sync::watch::Receiver<zeph_index::IndexProgress>>,
    /// Whether --tafc CLI flag was passed (overrides config).
    pub(crate) cli_tafc: bool,
    /// Set when TUI rendering was started early via `start_tui_early`.
    /// When `Some`, `run_tui_agent` skips creating a new TUI task and uses the existing one.
    pub(crate) early_tui: Option<EarlyTuiHandle>,
    /// Watch receiver for embed backfill progress.
    /// `None` = idle/done; `Some(p)` = backfill running with progress `p`.
    pub(crate) backfill_rx:
        tokio::sync::watch::Receiver<Option<zeph_memory::semantic::BackfillProgress>>,
    /// Optional supervisor passed to the TUI task registry panel (#2962).
    pub(crate) task_supervisor: Option<zeph_common::task_supervisor::TaskSupervisor>,
}

/// Phase-1 TUI handle: TUI is rendering but the agent hasn't started yet.
#[cfg(feature = "tui")]
pub(crate) struct EarlyTuiHandle {
    /// Oneshot receiver that fires when the TUI thread finishes.
    pub(crate) tui_done: tokio::sync::oneshot::Receiver<anyhow::Result<()>>,
    /// Send status/event updates to the TUI during setup.
    pub(crate) agent_tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
}

/// Start TUI rendering immediately (Phase 1).
///
/// Extracts `agent_rx` from `tui_handle`, creates the TUI `App`, spawns the rendering task,
/// and sends an initial "Starting up..." status message. The caller continues agent setup and
/// calls `run_tui_agent` (Phase 2) once ready.
///
/// Index progress forwarding is wired separately via `forward_index_progress_to_tui` once
/// `index_progress_rx` becomes available (after `apply_code_indexer`).
///
/// # Panics
///
/// Panics if `tui_handle.agent_rx` is `None` (already taken by a previous call).
#[cfg(feature = "tui")]
pub(crate) fn start_tui_early(
    tui_handle: &mut TuiHandle,
    config: &zeph_core::config::Config,
) -> EarlyTuiHandle {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
    let reader = zeph_tui::EventReader::new(event_tx, Duration::from_millis(100));
    std::thread::spawn(move || reader.run());

    let agent_rx = tui_handle
        .agent_rx
        .take()
        .expect("agent_rx already taken by start_tui_early");
    let mut tui_app = zeph_tui::App::new(tui_handle.user_tx.clone(), agent_rx)
        .with_command_tx(tui_handle.command_tx.clone());
    tui_app.set_show_source_labels(config.tui.show_source_labels);

    let agent_tx = tui_handle.agent_tx.clone();

    // Send initial loading status directly — channel is empty at this point (capacity 256).
    let _ = agent_tx.try_send(zeph_tui::AgentEvent::Status("Starting up...".into()));

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    std::thread::Builder::new()
        .name("zeph-tui".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tui runtime");
            let result = rt.block_on(async move {
                zeph_tui::run_tui(tui_app, event_rx).await?;
                Ok(())
            });
            let _ = done_tx.send(result);
        })
        .expect("spawn tui thread");

    EarlyTuiHandle {
        tui_done: done_rx,
        agent_tx,
    }
}

/// Warms up the provider, signals readiness, then shows embed backfill status until done.
///
/// After warmup completes and the "model ready" message clears, this task monitors
/// `backfill_rx` and keeps the TUI status bar showing "Backfilling embeddings..." until
/// the backfill finishes. This avoids the status being overwritten by subsequent init steps
/// during the startup sequence.
#[cfg(feature = "tui")]
async fn spawn_warmup_with_backfill_status(
    provider: AnyProvider,
    mut backfill_rx: tokio::sync::watch::Receiver<Option<zeph_memory::semantic::BackfillProgress>>,
    warmup_tx: tokio::sync::watch::Sender<bool>,
    tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
) {
    let _ = tx
        .send(zeph_tui::AgentEvent::Status("warming up model...".into()))
        .await;
    warmup_provider(&provider).await;
    let _ = tx
        .send(zeph_tui::AgentEvent::Status("model ready".into()))
        .await;
    let _ = warmup_tx.send(true);
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let _ = tx.send(zeph_tui::AgentEvent::Status(String::new())).await;
    // After init status clears, show backfill progress until it finishes.
    loop {
        let progress = *backfill_rx.borrow_and_update();
        if let Some(p) = progress {
            let pct = (p.done * 100).checked_div(p.total).unwrap_or(0);
            let _ = tx
                .send(zeph_tui::AgentEvent::Status(format!(
                    "Backfilling embeddings: {}/{} ({}%)",
                    p.done, p.total, pct
                )))
                .await;
        } else {
            let _ = tx.send(zeph_tui::AgentEvent::Status(String::new())).await;
            break;
        }
        if backfill_rx.changed().await.is_err() {
            break;
        }
    }
}

/// Spawn the TUI render thread (legacy path: no `EarlyTuiHandle`).
///
/// Creates the [`zeph_tui::App`], wires optional receivers, and spawns the TUI on a
/// dedicated OS thread with its own `current_thread` tokio runtime so that
/// `terminal.draw()` never blocks a shared tokio worker.
///
/// Returns a oneshot receiver that fires when the thread exits.
#[cfg(feature = "tui")]
// Cannot split: all arguments configure the App before the thread is spawned and there is
// no natural grouping that would not create an ad-hoc builder used only here.
#[allow(clippy::too_many_arguments)]
fn spawn_tui_thread(
    user_tx: tokio::sync::mpsc::Sender<String>,
    agent_rx: tokio::sync::mpsc::Receiver<zeph_tui::AgentEvent>,
    command_tx: tokio::sync::mpsc::Sender<zeph_tui::TuiCommand>,
    cancel_signal: std::sync::Arc<tokio::sync::Notify>,
    show_source_labels: bool,
    metrics_rx: Option<tokio::sync::watch::Receiver<zeph_core::metrics::MetricsSnapshot>>,
    task_supervisor: Option<zeph_common::task_supervisor::TaskSupervisor>,
    index_progress_rx: Option<tokio::sync::watch::Receiver<zeph_index::IndexProgress>>,
    agent_tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
) -> tokio::sync::oneshot::Receiver<anyhow::Result<()>> {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
    let reader = zeph_tui::EventReader::new(event_tx, Duration::from_millis(100));
    std::thread::spawn(move || reader.run());

    let mut tui_app = zeph_tui::App::new(user_tx, agent_rx)
        .with_cancel_signal(cancel_signal)
        .with_command_tx(command_tx);
    tui_app.set_show_source_labels(show_source_labels);

    if let Some(rx) = metrics_rx {
        tui_app = tui_app.with_metrics_rx(rx);
    }

    if let Some(supervisor) = task_supervisor {
        tui_app = tui_app.with_task_supervisor(supervisor);
    }

    if let Some(progress_rx) = index_progress_rx {
        tokio::spawn(forward_index_progress_to_tui(progress_rx, agent_tx));
    }

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    std::thread::Builder::new()
        .name("zeph-tui".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tui runtime");
            let result = rt.block_on(async move {
                zeph_tui::run_tui(tui_app, event_rx)
                    .await
                    .map_err(anyhow::Error::from)
            });
            let _ = done_tx.send(result);
        })
        .expect("spawn tui thread");
    done_rx
}

#[cfg(feature = "tui")]
fn tui_command_context(config: &zeph_core::config::Config, cli_tafc: bool) -> TuiCommandContext {
    TuiCommandContext {
        provider: format!("{:?}", config.llm.effective_provider()),
        model: config.llm.effective_model().to_owned(),
        agent_name: config.agent.name.clone(),
        semantic_enabled: config.memory.semantic.enabled,
        autonomy_level: format!("{:?}", config.security.autonomy_level),
        max_tool_iterations: config.agent.max_tool_iterations,
        tafc_enabled: config.tools.tafc.enabled || cli_tafc,
        tafc_complexity_threshold: config.tools.tafc.complexity_threshold,
        sandbox_backend: if config.tools.sandbox.enabled {
            #[cfg(target_os = "macos")]
            {
                Some("macos-seatbelt".to_owned())
            }
            #[cfg(all(target_os = "linux", feature = "sandbox"))]
            {
                Some("linux-bwrap-landlock".to_owned())
            }
            #[cfg(not(any(target_os = "macos", all(target_os = "linux", feature = "sandbox"))))]
            {
                Some("noop".to_owned())
            }
        } else {
            None
        },
        sandbox_denied_domains_count: config.tools.sandbox.denied_domains.len(),
        sandbox_fail_if_unavailable: config.tools.sandbox.fail_if_unavailable,
    }
}

#[cfg(feature = "tui")]
pub(crate) async fn run_tui_agent<C: Channel + 'static>(
    agent: zeph_core::agent::Agent<C>,
    mut params: TuiRunParams<'_>,
) -> anyhow::Result<()> {
    // Destructure handle fields needed regardless of path.
    let TuiHandle {
        user_tx,
        agent_tx: handle_agent_tx,
        agent_rx,
        command_tx,
        command_rx,
    } = params.tui_handle;

    // Determine TUI done-signal: reuse early-started thread or spawn a new one.
    let (tui_done, agent_tx) = if let Some(early) = params.early_tui {
        // Phase-2 path: TUI is already rendering. Wire cancel signal and metrics
        // into the running App via AgentEvent so Ctrl+C and metrics panel work correctly.
        drop(user_tx);
        drop(agent_rx);
        drop(command_tx);
        let _ = early
            .agent_tx
            .try_send(zeph_tui::AgentEvent::SetCancelSignal(agent.cancel_signal()));
        if let Some(metrics_rx) = params.metrics_rx {
            let _ = early
                .agent_tx
                .try_send(zeph_tui::AgentEvent::SetMetricsRx(metrics_rx));
        }
        (early.tui_done, early.agent_tx)
    } else {
        // Legacy path: TUI hasn't started yet, create App and spawn its thread now.
        let done_rx = spawn_tui_thread(
            user_tx,
            agent_rx.expect("agent_rx not set in TuiHandle"),
            command_tx,
            agent.cancel_signal(),
            params.config.tui.show_source_labels,
            params.metrics_rx.take(),
            params.task_supervisor.take(),
            params.index_progress_rx.take(),
            handle_agent_tx.clone(),
        );
        (done_rx, handle_agent_tx)
    };

    // Track all forwarding tasks so we can abort them when the agent exits,
    // ensuring the agent_event channel closes and the TUI thread quits.
    let mut forwarders = tokio::task::JoinSet::new();

    if let Some(rx) = params.status_rx {
        forwarders.spawn(forward_status_to_tui(rx, agent_tx.clone()));
    }
    // else: early forwarder already owns status_rx and is draining it
    forwarders.spawn(forward_tui_commands(
        command_rx,
        agent_tx.clone(),
        tui_command_context(params.config, params.cli_tafc),
    ));

    if let Some(tool_rx) = params.tool_rx {
        forwarders.spawn(forward_tool_events_to_tui(tool_rx, agent_tx.clone()));
    }

    let (warmup_tx, warmup_rx) = tokio::sync::watch::channel(false);
    forwarders.spawn(spawn_warmup_with_backfill_status(
        params.warmup_provider,
        params.backfill_rx,
        warmup_tx,
        agent_tx.clone(),
    ));

    let mut agent = agent.with_warmup_ready(warmup_rx);
    let agent_future = agent.run();

    tokio::select! {
        result = tui_done => {
            forwarders.abort_all();
            agent.shutdown().await;
            result.map_err(|_| anyhow::anyhow!("TUI thread exited without sending result"))??;
        }
        result = agent_future => {
            // Abort all forwarding tasks first, then drop our agent_tx clone.
            // Once all senders are gone the agent_event_rx channel closes, which
            // causes poll_agent_event to return None and the TUI thread to quit.
            forwarders.abort_all();
            drop(agent_tx);
            agent.shutdown().await;
            result?;
        }
    }

    Ok(())
}

pub(crate) async fn forward_status_to_stderr(mut rx: tokio::sync::mpsc::UnboundedReceiver<String>) {
    while let Some(msg) = rx.recv().await {
        eprintln!("[status] {msg}");
    }
}

// SECURITY: non-secret fields only
#[cfg(feature = "tui")]
pub(crate) struct TuiCommandContext {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) agent_name: String,
    pub(crate) semantic_enabled: bool,
    pub(crate) autonomy_level: String,
    pub(crate) max_tool_iterations: usize,
    pub(crate) tafc_enabled: bool,
    pub(crate) tafc_complexity_threshold: f64,
    /// Active sandbox backend name (e.g. `"macos-seatbelt"`, `"linux-bwrap-landlock"`, `"noop"`).
    /// `None` when sandbox is disabled.
    pub(crate) sandbox_backend: Option<String>,
    /// Number of entries in `[tools.sandbox].denied_domains`.
    pub(crate) sandbox_denied_domains_count: usize,
    /// Whether `fail_if_unavailable` is set in config.
    pub(crate) sandbox_fail_if_unavailable: bool,
}

#[cfg(feature = "tui")]
pub(crate) async fn forward_tui_commands(
    mut rx: tokio::sync::mpsc::Receiver<zeph_tui::TuiCommand>,
    tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
    ctx: TuiCommandContext,
) {
    while let Some(cmd) = rx.recv().await {
        let (command_id, output) = match cmd {
            zeph_tui::TuiCommand::ViewConfig => {
                let text = format!(
                    "Active configuration:\n  Provider: {}\n  Model: {}\n  Agent name: {}\n  Semantic enabled: {}",
                    ctx.provider, ctx.model, ctx.agent_name, ctx.semantic_enabled,
                );
                ("view:config".to_owned(), text)
            }
            zeph_tui::TuiCommand::ViewAutonomy => {
                let text = format!(
                    "Autonomy level: {}\n  Max tool iterations: {}",
                    ctx.autonomy_level, ctx.max_tool_iterations,
                );
                ("view:autonomy".to_owned(), text)
            }
            zeph_tui::TuiCommand::TafcStatus => {
                let text = if ctx.tafc_enabled {
                    format!(
                        "TAFC (Think-Augmented Function Calling): enabled\n  \
                         Complexity threshold: {:.2}\n  \
                         Note: changing TAFC settings mid-session causes a prompt cache miss.",
                        ctx.tafc_complexity_threshold,
                    )
                } else {
                    "TAFC (Think-Augmented Function Calling): disabled\n  \
                     Enable with --tafc CLI flag or [tools.tafc] enabled = true in config."
                        .to_owned()
                };
                ("tafc:status".to_owned(), text)
            }
            zeph_tui::TuiCommand::SandboxStatus => {
                let text = match &ctx.sandbox_backend {
                    None => "Sandbox: disabled\n  Set [tools.sandbox] enabled = true to enable."
                        .to_owned(),
                    Some(backend) => {
                        let egress = if ctx.sandbox_denied_domains_count == 0 {
                            "no denied domains configured".to_owned()
                        } else {
                            format!("{} denied domain(s)", ctx.sandbox_denied_domains_count)
                        };
                        let fail_str = if ctx.sandbox_fail_if_unavailable {
                            "yes"
                        } else {
                            "no"
                        };
                        format!(
                            "Sandbox: enabled\n  Backend: {backend}\n  \
                             Egress filter: {egress}\n  \
                             fail_if_unavailable: {fail_str}"
                        )
                    }
                };
                ("sandbox:status".to_owned(), text)
            }
            _ => continue,
        };
        if tx
            .send(zeph_tui::AgentEvent::CommandResult { command_id, output })
            .await
            .is_err()
        {
            break;
        }
    }
}

#[cfg(feature = "tui")]
pub(crate) async fn forward_status_to_tui(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
) {
    while let Some(msg) = rx.recv().await {
        if tx.send(zeph_tui::AgentEvent::Status(msg)).await.is_err() {
            break;
        }
    }
}

#[cfg(feature = "tui")]
pub(crate) async fn forward_tool_events_to_tui(
    mut rx: tokio::sync::mpsc::Receiver<zeph_tools::ToolEvent>,
    tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
) {
    // Only forward streaming chunks. ToolStart and ToolOutput are already sent via
    // TuiChannel::send_tool_start / send_tool_output from the Channel trait — forwarding
    // Started and Completed here would duplicate those events in the TUI.
    while let Some(event) = rx.recv().await {
        let agent_event = match event {
            zeph_tools::ToolEvent::Started { .. } | zeph_tools::ToolEvent::Completed { .. } => {
                continue;
            }
            zeph_tools::ToolEvent::OutputChunk {
                tool_name,
                command,
                chunk,
            } => zeph_tui::AgentEvent::ToolOutputChunk {
                tool_name,
                command,
                chunk: zeph_tools::strip_ansi(&chunk),
            },
            zeph_tools::ToolEvent::Rollback {
                restored_count,
                deleted_count,
                ..
            } => zeph_tui::AgentEvent::Status(format!(
                "Rolled back {restored_count} file(s), deleted {deleted_count} new file(s)"
            )),
        };
        if tx.send(agent_event).await.is_err() {
            break;
        }
    }
}

#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forward_status_to_tui_delivers_messages() {
        let (status_tx, status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<zeph_tui::AgentEvent>(16);

        tokio::spawn(forward_status_to_tui(status_rx, agent_tx));

        status_tx.send("Connecting tools...".into()).unwrap();
        status_tx.send("Memory ready".into()).unwrap();
        drop(status_tx);

        let mut received = Vec::new();
        while let Some(ev) = agent_rx.recv().await {
            received.push(ev);
        }

        assert_eq!(received.len(), 2);
        assert!(
            matches!(&received[0], zeph_tui::AgentEvent::Status(s) if s == "Connecting tools...")
        );
        assert!(matches!(&received[1], zeph_tui::AgentEvent::Status(s) if s == "Memory ready"));
    }

    #[tokio::test]
    async fn forward_status_to_tui_stops_when_agent_rx_dropped() {
        let (status_tx, status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<zeph_tui::AgentEvent>(1);

        let handle = tokio::spawn(forward_status_to_tui(status_rx, agent_tx));

        // Drop receiver — forwarder must exit cleanly when send fails.
        drop(agent_rx);

        status_tx.send("some status".into()).unwrap();
        // Give the forwarder a chance to detect the closed channel.
        handle.await.expect("forwarder panicked");
    }

    #[tokio::test]
    async fn forward_tool_events_skips_started_and_completed() {
        let (tool_tx, tool_rx) = tokio::sync::mpsc::channel::<zeph_tools::ToolEvent>(64);
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<zeph_tui::AgentEvent>(16);

        tokio::spawn(forward_tool_events_to_tui(tool_rx, agent_tx));

        tool_tx
            .send(zeph_tools::ToolEvent::Started {
                tool_name: "shell".into(),
                command: "ls".into(),
                sandbox_profile: None,
                resolved_cwd: None,
                execution_env: None,
            })
            .await
            .unwrap();
        tool_tx
            .send(zeph_tools::ToolEvent::OutputChunk {
                tool_name: "shell".into(),
                command: "ls".into(),
                chunk: "file.txt\n".into(),
            })
            .await
            .unwrap();
        tool_tx
            .send(zeph_tools::ToolEvent::Completed {
                tool_name: "shell".into(),
                command: "ls".into(),
                output: "file.txt\n".into(),
                success: true,
                diff: None,
                filter_stats: None,
                run_id: None,
            })
            .await
            .unwrap();
        drop(tool_tx);

        let mut received = Vec::new();
        while let Some(ev) = agent_rx.recv().await {
            received.push(ev);
        }

        assert_eq!(
            received.len(),
            1,
            "expected exactly one event (OutputChunk)"
        );
        assert!(
            matches!(received[0], zeph_tui::AgentEvent::ToolOutputChunk { .. }),
            "expected ToolOutputChunk, got {:?}",
            received[0]
        );
    }
}

#[cfg(feature = "tui")]
pub(crate) async fn forward_index_progress_to_tui(
    mut rx: tokio::sync::watch::Receiver<zeph_index::IndexProgress>,
    tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
) {
    let mut indexing_completed = false;
    while rx.changed().await.is_ok() {
        let p = rx.borrow_and_update().clone();
        if p.files_total == 0 {
            continue;
        }
        let msg = if p.files_done >= p.files_total {
            indexing_completed = true;
            format!(
                "Index ready ({} files, {} chunks)",
                p.files_total, p.chunks_created
            )
        } else {
            let pct = p.files_done * 100 / p.files_total;
            format!(
                "Indexing codebase... {}/{} files ({}%)",
                p.files_done, p.files_total, pct
            )
        };
        if tx.send(zeph_tui::AgentEvent::Status(msg)).await.is_err() {
            break;
        }
    }
    // Keep the final message visible briefly so the user can read it, then clear.
    // Use a shorter delay when indexing finished normally vs. when the sender was
    // dropped unexpectedly (e.g. error path) so the status bar does not stall.
    let delay = if indexing_completed {
        Duration::from_secs(1)
    } else {
        Duration::from_millis(200)
    };
    tokio::time::sleep(delay).await;
    let _ = tx.send(zeph_tui::AgentEvent::Status(String::new())).await;
}
