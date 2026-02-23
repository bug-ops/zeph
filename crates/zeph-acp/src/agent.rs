use std::pin::Pin;
use std::sync::Arc;

use agent_client_protocol::{
    AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification, ContentBlock,
    InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, ProtocolVersion,
    StopReason,
};
use tokio::sync::RwLock;
use zeph_core::channel::{ChannelMessage, LoopbackChannel};

use crate::bridge::bridge_loop;
use crate::error::AcpError;
use crate::session::SessionManager;

const MAX_PROMPT_BYTES: usize = 1_048_576; // 1 MiB

/// Factory closure type: receives a [`LoopbackChannel`] and runs the agent loop on it.
pub type AgentSpawner = Arc<
    dyn Fn(LoopbackChannel) -> Pin<Box<dyn std::future::Future<Output = ()> + 'static>> + 'static,
>;

pub struct ZephAcpAgent {
    pub(crate) session_mgr: Arc<SessionManager>,
    spawner: AgentSpawner,
    conn: Arc<RwLock<Option<agent_client_protocol::AgentSideConnection>>>,
}

impl ZephAcpAgent {
    #[must_use]
    pub fn new(spawner: AgentSpawner) -> Self {
        Self {
            session_mgr: Arc::new(SessionManager::new()),
            spawner,
            conn: Arc::new(RwLock::new(None)),
        }
    }

    /// Attach the live [`agent_client_protocol::AgentSideConnection`] after construction.
    pub async fn set_connection(&self, conn: agent_client_protocol::AgentSideConnection) {
        *self.conn.write().await = Some(conn);
    }
}

#[async_trait::async_trait(?Send)]
impl agent_client_protocol::Agent for ZephAcpAgent {
    async fn initialize(
        &self,
        _args: InitializeRequest,
    ) -> Result<InitializeResponse, agent_client_protocol::Error> {
        let caps = AgentCapabilities::new().load_session(false);
        Ok(InitializeResponse::new(ProtocolVersion::LATEST).agent_capabilities(caps))
    }

    async fn authenticate(
        &self,
        _args: AuthenticateRequest,
    ) -> Result<AuthenticateResponse, agent_client_protocol::Error> {
        // Phase 1 MVP: stdio transport only, auth delegated to host process.
        // TODO: validate credentials for network transport in Phase 2.
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(
        &self,
        _args: NewSessionRequest,
    ) -> Result<NewSessionResponse, agent_client_protocol::Error> {
        let session_id = uuid::Uuid::new_v4().to_string();
        tracing::debug!(session_id, "new ACP session");

        let (channel, handle, cancel_rx) = self
            .session_mgr
            .create(session_id.clone())
            .await
            .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))?;

        let spawner = Arc::clone(&self.spawner);
        let conn_arc = Arc::clone(&self.conn);
        let sid = session_id.clone();

        tokio::task::spawn_local(async move {
            let agent_fut = (spawner)(channel);
            let bridge_fut = async {
                if let Some(conn) = conn_arc.read().await.as_ref()
                    && let Err(e) = bridge_loop(conn, &sid, handle, cancel_rx).await
                {
                    tracing::warn!(session_id = %sid, error = %e, "bridge_loop error");
                }
            };

            tokio::join!(agent_fut, bridge_fut);
        });

        Ok(NewSessionResponse::new(session_id))
    }

    async fn prompt(
        &self,
        args: PromptRequest,
    ) -> Result<PromptResponse, agent_client_protocol::Error> {
        let session_id = args.session_id.to_string();

        let text = args
            .prompt
            .iter()
            .filter_map(|block| {
                if let ContentBlock::Text(t) = block {
                    Some(t.text.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        if text.len() > MAX_PROMPT_BYTES {
            return Err(agent_client_protocol::Error::invalid_request().data("prompt too large"));
        }

        let msg = ChannelMessage {
            text,
            attachments: vec![],
        };

        self.session_mgr
            .send_message(&session_id, msg)
            .await
            .map_err(|_| {
                agent_client_protocol::Error::internal_error().data("failed to deliver message")
            })?;

        Ok(PromptResponse::new(StopReason::EndTurn))
    }

    async fn cancel(&self, args: CancelNotification) -> Result<(), agent_client_protocol::Error> {
        let session_id = args.session_id.to_string();
        if let Err(AcpError::SessionNotFound(_)) = self.session_mgr.cancel(&session_id).await {
            // Session already gone — no-op per ACP spec.
        }
        Ok(())
    }

    async fn load_session(
        &self,
        args: LoadSessionRequest,
    ) -> Result<LoadSessionResponse, agent_client_protocol::Error> {
        let session_id = args.session_id.to_string();
        if self.session_mgr.exists(&session_id).await {
            Ok(LoadSessionResponse::new())
        } else {
            Err(agent_client_protocol::Error::internal_error().data("session not found"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn make_spawner() -> (AgentSpawner, Arc<AtomicBool>) {
        let called = Arc::new(AtomicBool::new(false));
        let called2 = Arc::clone(&called);
        let spawner: AgentSpawner = Arc::new(move |_channel| {
            let called3 = Arc::clone(&called2);
            Box::pin(async move {
                called3.store(true, Ordering::SeqCst);
            })
        });
        (spawner, called)
    }

    #[tokio::test]
    async fn new_session_creates_session() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (spawner, called) = make_spawner();
                let agent = ZephAcpAgent::new(spawner);
                use agent_client_protocol::Agent as _;
                let resp = agent
                    .new_session(NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let session_id = resp.session_id.to_string();
                assert!(!session_id.is_empty());
                assert!(agent.session_mgr.exists(&session_id).await);

                tokio::task::yield_now().await;
                assert!(called.load(Ordering::SeqCst));
            })
            .await;
    }

    #[tokio::test]
    async fn cancel_nonexistent_session_is_noop() {
        let (spawner, _) = make_spawner();
        let agent = ZephAcpAgent::new(spawner);
        use agent_client_protocol::Agent as _;
        agent
            .cancel(CancelNotification::new("nonexistent-session-id"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn initialize_returns_capabilities() {
        let (spawner, _) = make_spawner();
        let agent = ZephAcpAgent::new(spawner);
        use agent_client_protocol::Agent as _;
        let resp = agent
            .initialize(InitializeRequest::new(ProtocolVersion::LATEST))
            .await
            .unwrap();
        assert!(!resp.agent_capabilities.load_session);
    }

    #[tokio::test]
    async fn prompt_rejects_oversized_text() {
        let (spawner, _) = make_spawner();
        let agent = ZephAcpAgent::new(spawner);
        use agent_client_protocol::{Agent as _, ContentBlock, TextContent};

        let big_text = "x".repeat(MAX_PROMPT_BYTES + 1);
        let block = ContentBlock::Text(TextContent::new(big_text));
        let req = PromptRequest::new("sess-x", vec![block]);
        let result = agent.prompt(req).await;
        assert!(result.is_err());
    }

    // TODO: add test for prompt() happy path — Phase 2 item.
    // TODO: add test for load_session() error branch — Phase 2 item.
}
