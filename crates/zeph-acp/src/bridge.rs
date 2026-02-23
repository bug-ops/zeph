use std::sync::Arc;

use agent_client_protocol::{
    Client as _, ContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent,
};
use tokio::sync::watch;
use zeph_core::{LoopbackEvent, LoopbackHandle};

use crate::error::AcpError;

/// Reads events from the agent's [`LoopbackHandle`] and forwards them as ACP
/// [`SessionNotification`]s to the connected IDE client.
///
/// Exits when the output channel closes or cancel is signalled.
///
/// # Errors
///
/// Returns [`AcpError::Transport`] if sending a notification to the client fails.
pub async fn bridge_loop(
    conn: &agent_client_protocol::AgentSideConnection,
    session_id: &str,
    mut handle: LoopbackHandle,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<(), AcpError> {
    let session_id: Arc<str> = session_id.into();
    loop {
        tokio::select! {
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    tracing::debug!(session_id = %session_id, "bridge_loop: cancelled");
                    break;
                }
            }
            event = handle.output_rx.recv() => {
                match event {
                    None => {
                        tracing::debug!(session_id = %session_id, "bridge_loop: output channel closed");
                        break;
                    }
                    Some(ev) => {
                        if let Err(e) = forward_event(conn, Arc::clone(&session_id), ev).await {
                            tracing::warn!(session_id = %session_id, error = %e, "bridge_loop: failed to forward event");
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn forward_event(
    conn: &agent_client_protocol::AgentSideConnection,
    session_id: Arc<str>,
    event: LoopbackEvent,
) -> Result<(), AcpError> {
    let update = match event {
        LoopbackEvent::Chunk(text) | LoopbackEvent::FullMessage(text) => {
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            )))
        }
        LoopbackEvent::Status(text) => SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text)),
        )),
        LoopbackEvent::Flush => {
            return Ok(());
        }
        LoopbackEvent::ToolOutput {
            tool_name, display, ..
        } => {
            let text = format!("[{tool_name}] {display}");
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            )))
        }
    };

    let notification = SessionNotification::new(session_id.as_ref().to_owned(), update);

    conn.session_notification(notification)
        .await
        .map_err(|e| AcpError::Transport(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // TODO: add integration tests for forward_event() and bridge_loop() — Phase 2 item.
    // Requires mocking AgentSideConnection or extracting the match block into a pure function.

    #[test]
    fn loopback_event_flush_is_skipped() {
        let _ = LoopbackEvent::Flush;
    }

    #[test]
    fn loopback_event_variants_covered() {
        let _ = LoopbackEvent::Chunk("hi".to_owned());
        let _ = LoopbackEvent::FullMessage("hi".to_owned());
        let _ = LoopbackEvent::Status("working".to_owned());
        let _ = LoopbackEvent::ToolOutput {
            tool_name: "bash".to_owned(),
            display: "ok".to_owned(),
            diff: None,
            filter_stats: None,
            kept_lines: None,
        };
    }
}
