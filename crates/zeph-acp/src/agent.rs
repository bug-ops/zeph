use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol as acp;
use tokio::sync::{mpsc, oneshot};
use zeph_core::LoopbackEvent;
use zeph_core::channel::{ChannelMessage, LoopbackChannel};

use crate::fs::AcpFileExecutor;
use crate::permission::AcpPermissionGate;
use crate::terminal::AcpShellExecutor;
use crate::transport::ConnSlot;

const MAX_PROMPT_BYTES: usize = 1_048_576; // 1 MiB
const MAX_SESSIONS: usize = 1;
const LOOPBACK_CHANNEL_CAPACITY: usize = 64;

/// IDE-proxied capabilities passed to the agent loop per session.
///
/// Each field is `None` when the IDE did not advertise the corresponding capability.
pub struct AcpContext {
    pub file_executor: Option<AcpFileExecutor>,
    pub shell_executor: Option<AcpShellExecutor>,
    pub permission_gate: Option<AcpPermissionGate>,
}

/// Factory: receives a [`LoopbackChannel`] and optional [`AcpContext`], runs the agent loop.
pub type AgentSpawner = Arc<
    dyn Fn(
            LoopbackChannel,
            Option<AcpContext>,
        ) -> Pin<Box<dyn std::future::Future<Output = ()> + 'static>>
        + 'static,
>;

/// Sender half for delivering session notifications to the background writer.
pub(crate) type NotifySender =
    mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>;

struct SessionEntry {
    input_tx: mpsc::Sender<ChannelMessage>,
    // Receiver is owned solely by the prompt() handler; RefCell avoids Arc<Mutex> overhead
    // since MAX_SESSIONS=1 and prompt() is never called concurrently for the same session.
    output_rx: RefCell<Option<mpsc::Receiver<LoopbackEvent>>>,
}

pub struct ZephAcpAgent {
    notify_tx: NotifySender,
    spawner: AgentSpawner,
    sessions: RefCell<std::collections::HashMap<acp::SessionId, SessionEntry>>,
    conn_slot: ConnSlot,
    agent_name: String,
    agent_version: String,
    // IDE capabilities received during initialize(); used by build_acp_context.
    client_caps: RefCell<acp::ClientCapabilities>,
}

impl ZephAcpAgent {
    pub fn new(spawner: AgentSpawner, notify_tx: NotifySender, conn_slot: ConnSlot) -> Self {
        Self {
            notify_tx,
            spawner,
            sessions: RefCell::new(std::collections::HashMap::new()),
            conn_slot,
            agent_name: "zeph".to_owned(),
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            client_caps: RefCell::new(acp::ClientCapabilities::default()),
        }
    }

    #[must_use]
    pub fn with_agent_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.agent_name = name.into();
        self.agent_version = version.into();
        self
    }

    fn build_acp_context(&self, session_id: &acp::SessionId) -> Option<AcpContext> {
        let conn_guard = self.conn_slot.borrow();
        let conn = conn_guard.as_ref()?;

        let (perm_gate, perm_handler) = AcpPermissionGate::new(Rc::clone(conn));
        tokio::task::spawn_local(perm_handler);

        // Use actual IDE capabilities from initialize(); default to false (deny by default).
        let caps = self.client_caps.borrow();
        let can_read = caps.fs.read_text_file;
        let can_write = caps.fs.write_text_file;
        drop(caps);

        let (fs_exec, fs_handler) =
            AcpFileExecutor::new(Rc::clone(conn), session_id.clone(), can_read, can_write);
        tokio::task::spawn_local(fs_handler);

        let (shell_exec, shell_handler) =
            AcpShellExecutor::new(Rc::clone(conn), session_id.clone(), Some(perm_gate.clone()));
        tokio::task::spawn_local(shell_handler);

        Some(AcpContext {
            file_executor: Some(fs_exec),
            shell_executor: Some(shell_exec),
            permission_gate: Some(perm_gate),
        })
    }

    async fn send_notification(&self, notification: acp::SessionNotification) -> acp::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.notify_tx
            .send((notification, tx))
            .map_err(|_| acp::Error::internal_error().data("notification channel closed"))?;
        rx.await
            .map_err(|_| acp::Error::internal_error().data("notification ack lost"))
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for ZephAcpAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> acp::Result<acp::InitializeResponse> {
        tracing::debug!("ACP initialize");
        *self.client_caps.borrow_mut() = args.client_capabilities;
        let title = format!("{} AI Agent", self.agent_name);
        Ok(
            acp::InitializeResponse::new(acp::ProtocolVersion::LATEST).agent_info(
                acp::Implementation::new(&self.agent_name, &self.agent_version).title(title),
            ),
        )
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> acp::Result<acp::AuthenticateResponse> {
        // stdio transport: authentication is a no-op, IDE client is trusted.
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        _args: acp::NewSessionRequest,
    ) -> acp::Result<acp::NewSessionResponse> {
        if self.sessions.borrow().len() >= MAX_SESSIONS {
            return Err(acp::Error::internal_error().data("session limit reached"));
        }

        let session_id = acp::SessionId::new(uuid::Uuid::new_v4().to_string());
        tracing::debug!(%session_id, "new ACP session");

        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);

        let entry = SessionEntry {
            input_tx: handle.input_tx,
            output_rx: RefCell::new(Some(handle.output_rx)),
        };
        self.sessions.borrow_mut().insert(session_id.clone(), entry);

        let acp_ctx = self.build_acp_context(&session_id);
        let spawner = Arc::clone(&self.spawner);
        tokio::task::spawn_local(async move {
            (spawner)(channel, acp_ctx).await;
        });

        Ok(acp::NewSessionResponse::new(session_id))
    }

    async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
        tracing::debug!(session_id = %args.session_id, "ACP prompt");

        let mut text = String::new();
        for block in &args.prompt {
            if let acp::ContentBlock::Text(t) = block {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t.text);
            }
        }

        if text.len() > MAX_PROMPT_BYTES {
            return Err(acp::Error::invalid_request().data("prompt too large"));
        }

        let (input_tx, output_rx) = {
            let sessions = self.sessions.borrow();
            let entry = sessions
                .get(&args.session_id)
                .ok_or_else(|| acp::Error::internal_error().data("session not found"))?;
            let rx =
                entry.output_rx.borrow_mut().take().ok_or_else(|| {
                    acp::Error::internal_error().data("prompt already in progress")
                })?;
            (entry.input_tx.clone(), rx)
        };

        input_tx
            .send(ChannelMessage {
                text,
                attachments: vec![],
            })
            .await
            .map_err(|_| acp::Error::internal_error().data("agent channel closed"))?;

        // Block until the agent finishes this turn (signals via Flush or channel close).
        let mut rx = output_rx;
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

        // Return the receiver so future prompt() calls on this session can proceed.
        if let Some(entry) = self.sessions.borrow().get(&args.session_id) {
            *entry.output_rx.borrow_mut() = Some(rx);
        }

        Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
    }

    async fn cancel(&self, args: acp::CancelNotification) -> acp::Result<()> {
        tracing::debug!(session_id = %args.session_id, "ACP cancel");
        self.sessions.borrow_mut().remove(&args.session_id);
        Ok(())
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> acp::Result<acp::LoadSessionResponse> {
        if self.sessions.borrow().contains_key(&args.session_id) {
            Ok(acp::LoadSessionResponse::new())
        } else {
            Err(acp::Error::internal_error().data("session not found"))
        }
    }
}

fn loopback_event_to_update(event: LoopbackEvent) -> Option<acp::SessionUpdate> {
    match event {
        LoopbackEvent::Chunk(text) | LoopbackEvent::FullMessage(text) if text.is_empty() => None,
        LoopbackEvent::Chunk(text) | LoopbackEvent::FullMessage(text) => Some(
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(text.into())),
        ),
        LoopbackEvent::Status(text) if text.is_empty() => None,
        LoopbackEvent::Status(text) => Some(acp::SessionUpdate::AgentThoughtChunk(
            acp::ContentChunk::new(text.into()),
        )),
        LoopbackEvent::ToolOutput {
            tool_name, display, ..
        } => {
            let text = format!("[{tool_name}] {display}");
            Some(acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(text.into()),
            ))
        }
        LoopbackEvent::Flush => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spawner() -> AgentSpawner {
        Arc::new(|_channel, _ctx| Box::pin(async {}))
    }

    fn make_agent() -> (
        ZephAcpAgent,
        mpsc::UnboundedReceiver<(acp::SessionNotification, oneshot::Sender<()>)>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let conn_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        (ZephAcpAgent::new(make_spawner(), tx, conn_slot), rx)
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
                assert!(!resp.session_id.to_string().is_empty());
                assert!(agent.sessions.borrow().contains_key(&resp.session_id));
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
                let sid = resp.session_id.clone();
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

    #[test]
    fn loopback_empty_chunk_returns_none() {
        assert!(loopback_event_to_update(LoopbackEvent::Chunk(String::new())).is_none());
        assert!(loopback_event_to_update(LoopbackEvent::FullMessage(String::new())).is_none());
        assert!(loopback_event_to_update(LoopbackEvent::Status(String::new())).is_none());
    }

    #[test]
    fn loopback_tool_output_maps_to_agent_message() {
        let event = LoopbackEvent::ToolOutput {
            tool_name: "bash".to_owned(),
            display: "done".to_owned(),
            diff: None,
            filter_stats: None,
            kept_lines: None,
        };
        assert!(matches!(
            loopback_event_to_update(event),
            Some(acp::SessionUpdate::AgentMessageChunk(_))
        ));
    }

    #[tokio::test]
    async fn new_session_rejects_over_limit() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                // fill the limit
                agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                // second session should fail
                let res = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await;
                assert!(res.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn load_session_returns_ok_for_existing() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let res = agent
                    .load_session(acp::LoadSessionRequest::new(
                        resp.session_id,
                        std::path::PathBuf::from("."),
                    ))
                    .await;
                assert!(res.is_ok());
            })
            .await;
    }

    #[tokio::test]
    async fn load_session_errors_for_unknown() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let res = agent
                    .load_session(acp::LoadSessionRequest::new(
                        acp::SessionId::new("no-such"),
                        std::path::PathBuf::from("."),
                    ))
                    .await;
                assert!(res.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn prompt_errors_for_unknown_session() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let req = acp::PromptRequest::new("no-such", vec![]);
                assert!(agent.prompt(req).await.is_err());
            })
            .await;
    }
}
