// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "tui")]
use std::time::Duration;

#[cfg(feature = "tui")]
use crate::channel::TuiHandle;
#[cfg(feature = "tui")]
use zeph_core::bootstrap::warmup_provider;
#[cfg(feature = "tui")]
use zeph_core::channel::Channel;
#[cfg(feature = "tui")]
use zeph_llm::any::AnyProvider;

#[cfg(feature = "tui")]
pub(crate) struct TuiRunParams<'a> {
    pub(crate) tui_handle: TuiHandle,
    pub(crate) config: &'a zeph_core::config::Config,
    pub(crate) status_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    pub(crate) tool_rx: Option<tokio::sync::mpsc::UnboundedReceiver<zeph_tools::ToolEvent>>,
    pub(crate) metrics_rx:
        Option<tokio::sync::watch::Receiver<zeph_core::metrics::MetricsSnapshot>>,
    pub(crate) warmup_provider: AnyProvider,
    pub(crate) index_progress_rx: Option<tokio::sync::watch::Receiver<zeph_index::IndexProgress>>,
    /// Whether --tafc CLI flag was passed (overrides config).
    pub(crate) cli_tafc: bool,
    /// Set when TUI rendering was started early via `start_tui_early`.
    /// When `Some`, `run_tui_agent` skips creating a new TUI task and uses the existing one.
    pub(crate) early_tui: Option<EarlyTuiHandle>,
}

/// Phase-1 TUI handle: TUI is rendering but the agent hasn't started yet.
#[cfg(feature = "tui")]
pub(crate) struct EarlyTuiHandle {
    /// Join handle for the TUI rendering task.
    pub(crate) tui_task: tokio::task::JoinHandle<anyhow::Result<()>>,
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

    let tui_task = tokio::spawn(async move {
        zeph_tui::run_tui(tui_app, event_rx).await?;
        Ok(())
    });

    EarlyTuiHandle { tui_task, agent_tx }
}

#[cfg(feature = "tui")]
pub(crate) async fn run_tui_agent<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    params: TuiRunParams<'_>,
) -> anyhow::Result<()> {
    // Destructure handle fields needed regardless of path.
    let TuiHandle {
        user_tx,
        agent_tx: handle_agent_tx,
        agent_rx,
        command_tx,
        command_rx,
    } = params.tui_handle;

    // Determine TUI task: reuse early-started task or create new one.
    let (tui_task, agent_tx) = if let Some(early) = params.early_tui {
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
        (early.tui_task, early.agent_tx)
    } else {
        // Legacy path: TUI hasn't started yet, create App and task now.
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
        let reader = zeph_tui::EventReader::new(event_tx, Duration::from_millis(100));
        std::thread::spawn(move || reader.run());

        let rx = agent_rx.expect("agent_rx not set in TuiHandle");
        let mut tui_app = zeph_tui::App::new(user_tx, rx)
            .with_cancel_signal(agent.cancel_signal())
            .with_command_tx(command_tx);
        tui_app.set_show_source_labels(params.config.tui.show_source_labels);

        if let Some(metrics_rx) = params.metrics_rx {
            tui_app = tui_app.with_metrics_rx(metrics_rx);
        }

        if let Some(progress_rx) = params.index_progress_rx {
            tokio::spawn(forward_index_progress_to_tui(
                progress_rx,
                handle_agent_tx.clone(),
            ));
        }

        let task = tokio::spawn(async move {
            zeph_tui::run_tui(tui_app, event_rx)
                .await
                .map_err(anyhow::Error::from)
        });
        (task, handle_agent_tx)
    };

    // Note: when early_tui path is used, history is not pre-loaded into the TUI.
    // The chat view starts empty and fills as new messages arrive. This is an
    // accepted limitation — history replay is a separate enhancement.

    tokio::spawn(forward_status_to_tui(params.status_rx, agent_tx.clone()));
    tokio::spawn(forward_tui_commands(
        command_rx,
        agent_tx.clone(),
        TuiCommandContext {
            provider: format!("{:?}", params.config.llm.effective_provider()),
            model: params.config.llm.effective_model().to_owned(),
            agent_name: params.config.agent.name.clone(),
            semantic_enabled: params.config.memory.semantic.enabled,
            autonomy_level: format!("{:?}", params.config.security.autonomy_level),
            max_tool_iterations: params.config.agent.max_tool_iterations,
            tafc_enabled: params.config.tools.tafc.enabled || params.cli_tafc,
            tafc_complexity_threshold: params.config.tools.tafc.complexity_threshold,
        },
    ));

    if let Some(tool_rx) = params.tool_rx {
        tokio::spawn(forward_tool_events_to_tui(tool_rx, agent_tx.clone()));
    }

    let (warmup_tx, warmup_rx) = tokio::sync::watch::channel(false);
    let warmup_agent_tx = agent_tx.clone();
    let wp = params.warmup_provider;
    tokio::spawn(async move {
        let _ = warmup_agent_tx
            .send(zeph_tui::AgentEvent::Status("warming up model...".into()))
            .await;
        warmup_provider(&wp).await;
        let _ = warmup_agent_tx
            .send(zeph_tui::AgentEvent::Status("model ready".into()))
            .await;
        let _ = warmup_tx.send(true);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let _ = warmup_agent_tx
            .send(zeph_tui::AgentEvent::Status(String::new()))
            .await;
    });

    let mut agent = agent.with_warmup_ready(warmup_rx);
    let agent_future = agent.run();

    tokio::select! {
        result = tui_task => {
            agent.shutdown().await;
            result??;
        }
        result = agent_future => {
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
    mut rx: tokio::sync::mpsc::UnboundedReceiver<zeph_tools::ToolEvent>,
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
    async fn forward_tool_events_skips_started_and_completed() {
        let (tool_tx, tool_rx) = tokio::sync::mpsc::unbounded_channel::<zeph_tools::ToolEvent>();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<zeph_tui::AgentEvent>(16);

        tokio::spawn(forward_tool_events_to_tui(tool_rx, agent_tx));

        tool_tx
            .send(zeph_tools::ToolEvent::Started {
                tool_name: "shell".into(),
                command: "ls".into(),
            })
            .unwrap();
        tool_tx
            .send(zeph_tools::ToolEvent::OutputChunk {
                tool_name: "shell".into(),
                command: "ls".into(),
                chunk: "file.txt\n".into(),
            })
            .unwrap();
        tool_tx
            .send(zeph_tools::ToolEvent::Completed {
                tool_name: "shell".into(),
                command: "ls".into(),
                output: "file.txt\n".into(),
                success: true,
                diff: None,
                filter_stats: None,
            })
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
    while rx.changed().await.is_ok() {
        let p = rx.borrow_and_update().clone();
        if p.files_total == 0 {
            continue;
        }
        let msg = if p.files_done >= p.files_total {
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
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = tx.send(zeph_tui::AgentEvent::Status(String::new())).await;
}
