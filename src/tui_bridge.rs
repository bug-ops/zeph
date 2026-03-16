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
}

#[cfg(feature = "tui")]
pub(crate) async fn run_tui_agent<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    params: TuiRunParams<'_>,
) -> anyhow::Result<()> {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);

    let reader = zeph_tui::EventReader::new(event_tx, Duration::from_millis(100));
    std::thread::spawn(move || reader.run());

    let mut tui_app = zeph_tui::App::new(params.tui_handle.user_tx, params.tui_handle.agent_rx)
        .with_cancel_signal(agent.cancel_signal())
        .with_command_tx(params.tui_handle.command_tx);
    tui_app.set_show_source_labels(params.config.tui.show_source_labels);

    let history: Vec<(&str, &str)> = agent
        .context_messages()
        .iter()
        .map(|m| {
            let role = match m.role {
                zeph_llm::provider::Role::User => "user",
                zeph_llm::provider::Role::Assistant => "assistant",
                zeph_llm::provider::Role::System => "system",
            };
            (role, m.content.as_str())
        })
        .collect();
    tui_app.load_history(&history);

    if let Some(rx) = params.metrics_rx {
        tui_app = tui_app.with_metrics_rx(rx);
    }

    let agent_tx = params.tui_handle.agent_tx;
    tokio::spawn(forward_status_to_tui(params.status_rx, agent_tx.clone()));
    tokio::spawn(forward_tui_commands(
        params.tui_handle.command_rx,
        agent_tx.clone(),
        TuiCommandContext {
            provider: format!("{:?}", params.config.llm.provider),
            model: params.config.llm.model.clone(),
            agent_name: params.config.agent.name.clone(),
            semantic_enabled: params.config.memory.semantic.enabled,
            autonomy_level: format!("{:?}", params.config.security.autonomy_level),
            max_tool_iterations: params.config.agent.max_tool_iterations,
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

    if let Some(rx) = params.index_progress_rx {
        tokio::spawn(forward_index_progress_to_tui(rx, agent_tx.clone()));
    }

    let mut agent = agent.with_warmup_ready(warmup_rx);

    let tui_task = tokio::spawn(zeph_tui::run_tui(tui_app, event_rx));
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
    while let Some(event) = rx.recv().await {
        let agent_event = match event {
            zeph_tools::ToolEvent::Started { tool_name, command } => {
                zeph_tui::AgentEvent::ToolStart { tool_name, command }
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
            zeph_tools::ToolEvent::Completed {
                tool_name,
                command,
                output,
                success,
                diff,
                filter_stats,
            } => {
                let stats_line = filter_stats.as_ref().and_then(|fs| {
                    (fs.filtered_chars < fs.raw_chars)
                        .then(|| format!("{:.1}% filtered", fs.savings_pct()))
                });
                let kept = filter_stats
                    .as_ref()
                    .filter(|fs| !fs.kept_lines.is_empty())
                    .map(|fs| fs.kept_lines.clone());
                zeph_tui::AgentEvent::ToolOutput {
                    tool_name,
                    command,
                    output,
                    success,
                    diff,
                    filter_stats: stats_line,
                    kept_lines: kept,
                }
            }
        };
        if tx.send(agent_event).await.is_err() {
            break;
        }
    }
}

#[cfg(feature = "tui")]
async fn forward_index_progress_to_tui(
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
