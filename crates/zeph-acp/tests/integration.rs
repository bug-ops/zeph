use std::path::PathBuf;
use std::sync::Arc;

use acp::Agent as _;
use agent_client_protocol as acp;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use zeph_acp::{AcpServerConfig, AgentSpawner, serve_connection};
use zeph_core::channel::LoopbackChannel;

/// No-op ACP client for testing.
struct NoopClient;

#[async_trait::async_trait(?Send)]
impl acp::Client for NoopClient {
    async fn request_permission(
        &self,
        _args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Cancelled,
        ))
    }

    async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
        Ok(())
    }
}

fn make_echo_spawner() -> AgentSpawner {
    Arc::new(|_channel: LoopbackChannel, _ctx| Box::pin(async {}))
}

fn make_server_config() -> AcpServerConfig {
    AcpServerConfig {
        agent_name: "test-agent".to_owned(),
        agent_version: "0.0.1".to_owned(),
        max_sessions: 4,
        session_idle_timeout_secs: 1800,
    }
}

#[tokio::test]
async fn initialize_handshake() {
    let (client_stream, server_stream) = duplex(65536);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (server_read, server_write) = tokio::io::split(server_stream);

    let spawner = make_echo_spawner();
    let server_config = make_server_config();

    let server_fut = serve_connection(
        spawner,
        server_config,
        server_write.compat_write(),
        server_read.compat(),
    );

    let client_fut = async {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_conn, io_fut) = acp::ClientSideConnection::new(
                    NoopClient,
                    client_write.compat_write(),
                    client_read.compat(),
                    |fut| {
                        tokio::task::spawn_local(fut);
                    },
                );
                tokio::task::spawn_local(async move {
                    let _ = io_fut.await;
                });

                let resp = client_conn
                    .initialize(acp::InitializeRequest::new(acp::ProtocolVersion::LATEST))
                    .await
                    .expect("initialize failed");

                assert!(resp.agent_info.is_some());
                let info = resp.agent_info.unwrap();
                assert_eq!(info.name, "test-agent");
                assert_eq!(info.version, "0.0.1");
            })
            .await;
    };

    tokio::select! {
        res = server_fut => {
            // Server can exit normally after client disconnects
            let _ = res;
        }
        _ = client_fut => {}
    }
}

#[tokio::test]
async fn new_session_and_cancel() {
    let (client_stream, server_stream) = duplex(65536);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (server_read, server_write) = tokio::io::split(server_stream);

    let spawner = make_echo_spawner();
    let server_config = make_server_config();

    let server_fut = serve_connection(
        spawner,
        server_config,
        server_write.compat_write(),
        server_read.compat(),
    );

    let client_fut = async {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_conn, io_fut) = acp::ClientSideConnection::new(
                    NoopClient,
                    client_write.compat_write(),
                    client_read.compat(),
                    |fut| {
                        tokio::task::spawn_local(fut);
                    },
                );
                tokio::task::spawn_local(async move {
                    let _ = io_fut.await;
                });

                client_conn
                    .initialize(acp::InitializeRequest::new(acp::ProtocolVersion::LATEST))
                    .await
                    .expect("initialize failed");

                let session_resp = client_conn
                    .new_session(acp::NewSessionRequest::new(PathBuf::from(".")))
                    .await
                    .expect("new_session failed");

                let session_id = session_resp.session_id.clone();
                assert!(!session_id.to_string().is_empty());

                client_conn
                    .cancel(acp::CancelNotification::new(session_id))
                    .await
                    .expect("cancel failed");
            })
            .await;
    };

    tokio::select! {
        res = server_fut => {
            let _ = res;
        }
        _ = client_fut => {}
    }
}
