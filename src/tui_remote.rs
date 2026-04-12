// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(all(feature = "tui", feature = "a2a"))]
use zeph_tui::{App, EventReader};

#[cfg(all(feature = "tui", feature = "a2a"))]
pub(crate) async fn run_tui_remote(
    url: String,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use futures::StreamExt;
    use std::time::Duration;

    let config_file = crate::bootstrap::resolve_config_path(config_path);
    let config = zeph_core::config::Config::load(&config_file)
        .unwrap_or_else(|_| zeph_core::config::Config::default());
    config.validate()?;
    let auth_token = config.a2a.auth_token.clone();

    let client = zeph_a2a::A2aClient::new(zeph_core::http::default_client());

    // user_tx is passed to App; App sends user text through it.
    // We receive on user_rx and forward to the A2A SSE pump.
    let (user_tx, mut user_rx) = tokio::sync::mpsc::channel::<String>(32);
    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<zeph_tui::AgentEvent>(256);

    let agent_tx_pump = agent_tx.clone();
    tokio::spawn(async move {
        while let Some(text) = user_rx.recv().await {
            let message = zeph_a2a::Message::user_text(&text);
            let params = zeph_a2a::SendMessageParams {
                message,
                configuration: None,
            };

            let _ = agent_tx_pump.send(zeph_tui::AgentEvent::Typing).await;

            let stream_result = client
                .stream_message(&url, params, auth_token.as_deref())
                .await;

            match stream_result {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        match event {
                            Ok(zeph_a2a::TaskEvent::ArtifactUpdate(artifact_evt)) => {
                                let text: String = artifact_evt
                                    .artifact
                                    .parts
                                    .iter()
                                    .filter_map(|p| {
                                        if let zeph_a2a::Part::Text { text, .. } = p {
                                            Some(text.as_str())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();
                                let is_final = artifact_evt.is_final;
                                if !text.is_empty() {
                                    let _ =
                                        agent_tx_pump.send(zeph_tui::AgentEvent::Chunk(text)).await;
                                }
                                if is_final {
                                    let _ = agent_tx_pump.send(zeph_tui::AgentEvent::Flush).await;
                                }
                            }
                            Ok(zeph_a2a::TaskEvent::StatusUpdate(status_evt)) => {
                                match status_evt.status.state {
                                    zeph_a2a::TaskState::Completed => {
                                        let _ =
                                            agent_tx_pump.send(zeph_tui::AgentEvent::Flush).await;
                                    }
                                    zeph_a2a::TaskState::Failed => {
                                        let _ = agent_tx_pump
                                            .send(zeph_tui::AgentEvent::FullMessage(
                                                "Error: task failed".into(),
                                            ))
                                            .await;
                                    }
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                let _ = agent_tx_pump
                                    .send(zeph_tui::AgentEvent::FullMessage(format!(
                                        "Connection error: {e}"
                                    )))
                                    .await;
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = agent_tx_pump
                        .send(zeph_tui::AgentEvent::FullMessage(format!(
                            "Connection error: {e}"
                        )))
                        .await;
                }
            }
        }
    });

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
    let reader = EventReader::new(event_tx, Duration::from_millis(100));
    std::thread::spawn(move || reader.run());

    let mut tui_app = App::new(user_tx, agent_rx);
    tui_app.set_show_source_labels(config.tui.show_source_labels);

    zeph_tui::run_tui(tui_app, event_rx).await?;
    Ok(())
}
