// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;
use std::sync::Arc;

use acp::Agent as _;
use agent_client_protocol as acp;
use serde_json::value::RawValue;
use tokio::io::duplex;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use zeph_acp::{AcpServerConfig, AgentSpawner, SessionContext, serve_connection};
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
    Arc::new(|_channel: LoopbackChannel, _ctx, _session_ctx: SessionContext| Box::pin(async {}))
}

fn make_server_config() -> AcpServerConfig {
    AcpServerConfig {
        agent_name: "test-agent".to_owned(),
        agent_version: "0.0.1".to_owned(),
        max_sessions: 4,
        session_idle_timeout_secs: 1800,
        permission_file: None,
        provider_factory: None,
        available_models: Vec::new(),
        mcp_manager: None,
        auth_bearer_token: None,
        discovery_enabled: true,
        terminal_timeout_secs: 120,
        project_rules: Vec::new(),
        title_max_chars: 60,
        max_history: 100,
        sqlite_path: None,
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

// ── E2E: Custom methods via JSON-RPC transport ───────────────────────────────

fn raw_json(json: &str) -> Arc<RawValue> {
    Arc::from(RawValue::from_string(json.to_owned()).unwrap())
}

/// Helper: set up client+server duplex, initialize, and run `test_fn` with the client connection.
async fn with_initialized_client<F, Fut>(test_fn: F)
where
    F: FnOnce(acp::ClientSideConnection) -> Fut + 'static,
    Fut: std::future::Future<Output = ()>,
{
    let (client_stream, server_stream) = duplex(65536);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (server_read, server_write) = tokio::io::split(server_stream);

    let server_fut = serve_connection(
        make_echo_spawner(),
        make_server_config(),
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

                test_fn(client_conn).await;
            })
            .await;
    };

    tokio::select! {
        res = server_fut => { let _ = res; }
        _ = client_fut => {}
    }
}

#[tokio::test]
async fn e2e_session_list_empty() {
    with_initialized_client(|conn| async move {
        let resp = conn
            .ext_method(acp::ExtRequest::new("_session/list", raw_json("{}")))
            .await
            .expect("ext_method failed");
        let parsed: serde_json::Value = serde_json::from_str(resp.0.get()).unwrap();
        assert!(parsed["sessions"].as_array().unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn e2e_session_list_includes_created_session() {
    with_initialized_client(|conn| async move {
        let session = conn
            .new_session(acp::NewSessionRequest::new(PathBuf::from(".")))
            .await
            .expect("new_session failed");
        let sid = session.session_id.to_string();

        let resp = conn
            .ext_method(acp::ExtRequest::new("_session/list", raw_json("{}")))
            .await
            .expect("ext_method failed");
        let parsed: serde_json::Value = serde_json::from_str(resp.0.get()).unwrap();
        let sessions = parsed["sessions"].as_array().unwrap();
        assert!(sessions.iter().any(|s| s["session_id"] == sid));
    })
    .await;
}

#[tokio::test]
async fn e2e_session_delete_and_verify() {
    with_initialized_client(|conn| async move {
        let session = conn
            .new_session(acp::NewSessionRequest::new(PathBuf::from(".")))
            .await
            .expect("new_session failed");
        let sid = session.session_id.to_string();

        let json = format!(r#"{{"session_id":"{sid}"}}"#);
        let resp = conn
            .ext_method(acp::ExtRequest::new("_session/delete", raw_json(&json)))
            .await
            .expect("ext_method failed");
        let parsed: serde_json::Value = serde_json::from_str(resp.0.get()).unwrap();
        assert_eq!(parsed["deleted"], true);

        // Verify session no longer in list.
        let list_resp = conn
            .ext_method(acp::ExtRequest::new("_session/list", raw_json("{}")))
            .await
            .expect("ext_method failed");
        let list: serde_json::Value = serde_json::from_str(list_resp.0.get()).unwrap();
        assert!(
            !list["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["session_id"] == sid)
        );
    })
    .await;
}

#[tokio::test]
async fn e2e_agent_tools_returns_list() {
    with_initialized_client(|conn| async move {
        let resp = conn
            .ext_method(acp::ExtRequest::new(
                "_agent/tools",
                raw_json(r#"{"session_id":"any"}"#),
            ))
            .await
            .expect("ext_method failed");
        let parsed: serde_json::Value = serde_json::from_str(resp.0.get()).unwrap();
        assert!(!parsed["tools"].as_array().unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn e2e_working_dir_update_and_path_traversal() {
    with_initialized_client(|conn| async move {
        let session = conn
            .new_session(acp::NewSessionRequest::new(PathBuf::from(".")))
            .await
            .expect("new_session failed");
        let sid = session.session_id.to_string();

        // Valid path update.
        let json = format!(r#"{{"session_id":"{sid}","path":"/tmp/workspace"}}"#);
        let resp = conn
            .ext_method(acp::ExtRequest::new(
                "_agent/working_dir/update",
                raw_json(&json),
            ))
            .await
            .expect("ext_method failed");
        let parsed: serde_json::Value = serde_json::from_str(resp.0.get()).unwrap();
        assert_eq!(parsed["updated"], true);

        // Path traversal must be rejected.
        let bad_json = format!(r#"{{"session_id":"{sid}","path":"../../etc/passwd"}}"#);
        let err = conn
            .ext_method(acp::ExtRequest::new(
                "_agent/working_dir/update",
                raw_json(&bad_json),
            ))
            .await;
        assert!(err.is_err());
    })
    .await;
}

#[tokio::test]
async fn e2e_session_import_export_roundtrip() {
    with_initialized_client(|conn| async move {
        // Import events.
        let import_json = r#"{"events":[{"event_type":"user_message","payload":"hello"}]}"#;
        let resp = conn
            .ext_method(acp::ExtRequest::new(
                "_session/import",
                raw_json(import_json),
            ))
            .await
            .expect("import failed");
        let import_resp: serde_json::Value = serde_json::from_str(resp.0.get()).unwrap();
        let new_sid = import_resp["session_id"].as_str().unwrap();
        assert!(!new_sid.is_empty());

        // Export (no store, so events will be empty — but method should succeed).
        let export_json = format!(r#"{{"session_id":"{new_sid}"}}"#);
        let resp = conn
            .ext_method(acp::ExtRequest::new(
                "_session/export",
                raw_json(&export_json),
            ))
            .await
            .expect("export failed");
        let export_resp: serde_json::Value = serde_json::from_str(resp.0.get()).unwrap();
        assert_eq!(export_resp["session_id"], new_sid);
        assert!(export_resp["exported_at"].as_str().is_some());
    })
    .await;
}

#[tokio::test]
async fn e2e_unknown_ext_method_returns_null() {
    with_initialized_client(|conn| async move {
        let resp = conn
            .ext_method(acp::ExtRequest::new("unknown/method", raw_json("{}")))
            .await
            .expect("ext_method failed");
        assert_eq!(resp.0.get(), "null");
    })
    .await;
}

#[tokio::test]
async fn e2e_initialize_returns_auth_hint_and_load_session() {
    let (client_stream, server_stream) = duplex(65536);
    let (client_read, client_write) = tokio::io::split(client_stream);
    let (server_read, server_write) = tokio::io::split(server_stream);

    let server_fut = serve_connection(
        make_echo_spawner(),
        make_server_config(),
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

                // Verify auth_hint in meta.
                let meta = resp.meta.expect("meta should be present");
                assert!(meta.contains_key("auth_hint"));

                // Verify load_session capability.
                assert!(resp.agent_capabilities.load_session);
            })
            .await;
    };

    tokio::select! {
        res = server_fut => { let _ = res; }
        _ = client_fut => {}
    }
}

#[tokio::test]
async fn e2e_new_session_includes_available_modes() {
    with_initialized_client(|conn| async move {
        let resp = conn
            .new_session(acp::NewSessionRequest::new(PathBuf::from(".")))
            .await
            .expect("new_session failed");
        let modes = resp
            .modes
            .expect("modes field must be present in new_session response");
        assert_eq!(modes.current_mode_id.0.as_ref(), "code");
        assert_eq!(modes.available_modes.len(), 3);
        let ids: Vec<&str> = modes
            .available_modes
            .iter()
            .map(|m| m.id.0.as_ref())
            .collect();
        assert!(ids.contains(&"code"));
        assert!(ids.contains(&"architect"));
        assert!(ids.contains(&"ask"));
    })
    .await;
}

#[tokio::test]
async fn e2e_set_session_mode_success() {
    with_initialized_client(|conn| async move {
        let session = conn
            .new_session(acp::NewSessionRequest::new(PathBuf::from(".")))
            .await
            .expect("new_session failed");

        conn.set_session_mode(acp::SetSessionModeRequest::new(
            session.session_id,
            "architect",
        ))
        .await
        .expect("set_session_mode failed");
    })
    .await;
}

#[tokio::test]
async fn e2e_set_session_mode_rejects_invalid_mode() {
    with_initialized_client(|conn| async move {
        let session = conn
            .new_session(acp::NewSessionRequest::new(PathBuf::from(".")))
            .await
            .expect("new_session failed");

        let err = conn
            .set_session_mode(acp::SetSessionModeRequest::new(
                session.session_id,
                "nonexistent-mode",
            ))
            .await;
        assert!(err.is_err(), "unknown mode must be rejected");
    })
    .await;
}

#[tokio::test]
async fn e2e_set_session_mode_rejects_unknown_session() {
    with_initialized_client(|conn| async move {
        let err = conn
            .set_session_mode(acp::SetSessionModeRequest::new(
                acp::SessionId::new("no-such-session"),
                "code",
            ))
            .await;
        assert!(err.is_err(), "unknown session must be rejected");
    })
    .await;
}
