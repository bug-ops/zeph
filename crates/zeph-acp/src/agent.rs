use std::cell::Cell;
use std::pin::Pin;
use std::sync::Arc;

use agent_client_protocol as acp;
use tokio::sync::{mpsc, oneshot};
use zeph_core::LoopbackEvent;
use zeph_core::channel::{ChannelMessage, LoopbackChannel};

use crate::error::AcpError;

const MAX_PROMPT_BYTES: usize = 1_048_576; // 1 MiB
const MAX_SESSIONS: usize = 100;

/// Factory: receives a [`LoopbackChannel`] and runs the agent loop on it.
pub type AgentSpawner = Arc<
    dyn Fn(LoopbackChannel) -> Pin<Box<dyn std::future::Future<Output = ()> + 'static>> + 'static,
>;

/// Sender half for delivering session notifications to the background writer.
pub(crate) type NotifySender =
    mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>;

struct SessionEntry {
    input_tx: mpsc::Sender<ChannelMessage>,
    output_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<LoopbackEvent>>>,
}

pub struct ZephAcpAgent {
    notify_tx: NotifySender,
    spawner: AgentSpawner,
    sessions: std::cell::RefCell<std::collections::HashMap<String, SessionEntry>>,
    next_id: Cell<u64>,
}

impl ZephAcpAgent {
    pub fn new(spawner: AgentSpawner, notify_tx: NotifySender) -> Self {
        Self {
            notify_tx,
            spawner,
            sessions: std::cell::RefCell::new(std::collections::HashMap::new()),
            next_id: Cell::new(0),
        }
    }

    async fn send_notification(
        &self,
        notification: acp::SessionNotification,
    ) -> Result<(), AcpError> {
        let (tx, rx) = oneshot::channel();
        self.notify_tx
            .send((notification, tx))
            .map_err(|_| AcpError::Transport("notification channel closed".to_owned()))?;
        rx.await
            .map_err(|_| AcpError::Transport("notification ack lost".to_owned()))
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for ZephAcpAgent {
    async fn initialize(
        &self,
        _args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        tracing::debug!("ACP initialize");
        Ok(
            acp::InitializeResponse::new(acp::ProtocolVersion::LATEST).agent_info(
                acp::Implementation::new("zeph", env!("CARGO_PKG_VERSION")).title("Zeph AI Agent"),
            ),
        )
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        // stdio transport: authentication is a no-op, IDE client is trusted.
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        _args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        if self.sessions.borrow().len() >= MAX_SESSIONS {
            return Err(acp::Error::internal_error().data("session limit reached"));
        }

        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let session_id = uuid::Uuid::new_v4().to_string();
        tracing::debug!(session_id, "new ACP session");

        let (channel, handle) = LoopbackChannel::pair(64);

        let entry = SessionEntry {
            input_tx: handle.input_tx,
            output_rx: Arc::new(tokio::sync::Mutex::new(handle.output_rx)),
        };
        self.sessions.borrow_mut().insert(session_id.clone(), entry);

        let spawner = Arc::clone(&self.spawner);
        tokio::task::spawn_local(async move {
            (spawner)(channel).await;
        });

        Ok(acp::NewSessionResponse::new(session_id))
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        let session_id = args.session_id.to_string();
        tracing::debug!(session_id, "ACP prompt");

        let text = args
            .prompt
            .iter()
            .filter_map(|block| {
                if let acp::ContentBlock::Text(t) = block {
                    Some(t.text.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        if text.len() > MAX_PROMPT_BYTES {
            return Err(acp::Error::invalid_request().data("prompt too large"));
        }

        let (input_tx, output_rx) = {
            let sessions = self.sessions.borrow();
            let entry = sessions
                .get(&session_id)
                .ok_or_else(|| acp::Error::internal_error().data("session not found"))?;
            (entry.input_tx.clone(), Arc::clone(&entry.output_rx))
        };

        input_tx
            .send(ChannelMessage {
                text,
                attachments: vec![],
            })
            .await
            .map_err(|_| acp::Error::internal_error().data("agent channel closed"))?;

        // Block until the agent finishes this turn (signals via Flush or channel close).
        let mut rx = output_rx.lock().await;
        while let Some(event) = rx.recv().await {
            let is_flush = matches!(event, LoopbackEvent::Flush);
            if let Some(update) = loopback_event_to_update(event) {
                let notification = acp::SessionNotification::new(args.session_id.clone(), update);
                if let Err(e) = self.send_notification(notification).await {
                    tracing::warn!(error = %e, "failed to send notification");
                    break;
                }
            }
            if is_flush {
                break;
            }
        }

        Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
    }

    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        let session_id = args.session_id.to_string();
        tracing::debug!(session_id, "ACP cancel");
        self.sessions.borrow_mut().remove(&session_id);
        Ok(())
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        let session_id = args.session_id.to_string();
        if self.sessions.borrow().contains_key(&session_id) {
            Ok(acp::LoadSessionResponse::new())
        } else {
            Err(acp::Error::internal_error().data("session not found"))
        }
    }
}

fn loopback_event_to_update(event: LoopbackEvent) -> Option<acp::SessionUpdate> {
    match event {
        LoopbackEvent::Chunk(text) | LoopbackEvent::FullMessage(text) => {
            Some(acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text))),
            ))
        }
        LoopbackEvent::Status(text) => Some(acp::SessionUpdate::AgentThoughtChunk(
            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text))),
        )),
        LoopbackEvent::ToolOutput {
            tool_name, display, ..
        } => {
            let text = format!("[{tool_name}] {display}");
            Some(acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text))),
            ))
        }
        LoopbackEvent::Flush => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spawner() -> AgentSpawner {
        Arc::new(|_channel| Box::pin(async {}))
    }

    fn make_agent() -> (
        ZephAcpAgent,
        mpsc::UnboundedReceiver<(acp::SessionNotification, oneshot::Sender<()>)>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ZephAcpAgent::new(make_spawner(), tx), rx)
    }

    #[tokio::test]
    async fn initialize_returns_agent_info() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .initialize(acp::InitializeRequest::new(acp::ProtocolVersion::LATEST))
                    .await
                    .unwrap();
                assert!(resp.agent_info.is_some());
            })
            .await;
    }

    #[tokio::test]
    async fn new_session_creates_entry() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.to_string();
                assert!(!sid.is_empty());
                assert!(agent.sessions.borrow().contains_key(&sid));
            })
            .await;
    }

    #[tokio::test]
    async fn cancel_removes_session() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.to_string();
                agent
                    .cancel(acp::CancelNotification::new(sid.clone()))
                    .await
                    .unwrap();
                assert!(!agent.sessions.borrow().contains_key(&sid));
            })
            .await;
    }

    #[tokio::test]
    async fn prompt_rejects_oversized() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let big = "x".repeat(MAX_PROMPT_BYTES + 1);
                let block = acp::ContentBlock::Text(acp::TextContent::new(big));
                let req = acp::PromptRequest::new(resp.session_id.to_string(), vec![block]);
                assert!(agent.prompt(req).await.is_err());
            })
            .await;
    }

    #[test]
    fn loopback_flush_returns_none() {
        assert!(loopback_event_to_update(LoopbackEvent::Flush).is_none());
    }

    #[test]
    fn loopback_chunk_maps_to_agent_message() {
        assert!(matches!(
            loopback_event_to_update(LoopbackEvent::Chunk("hi".into())),
            Some(acp::SessionUpdate::AgentMessageChunk(_))
        ));
    }

    #[test]
    fn loopback_status_maps_to_thought() {
        assert!(matches!(
            loopback_event_to_update(LoopbackEvent::Status("thinking".into())),
            Some(acp::SessionUpdate::AgentThoughtChunk(_))
        ));
    }
}
