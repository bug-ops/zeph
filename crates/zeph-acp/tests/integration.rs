// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
// Raised from 128: async-fn state machine chain through serve_connection/run_agent handlers
// deepens past the default depth limit under release-profile query evaluation.
#![recursion_limit = "256"]

//! Integration tests for the ACP 0.11 server (`zeph-acp`) using in-process loopback transports.
//!
//! These tests exercise the full ACP protocol stack: `serve_connection` → `run_agent` →
//! request handlers, driven by a real `acp::Client` over a `tokio::io::duplex` byte stream.
//! Each test runs inside a `tokio::task::LocalSet` because the agent session futures are `!Send`.

use std::sync::Arc;

use agent_client_protocol as acp;
use tempfile::TempDir;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use zeph_acp::{AcpServerConfig, AgentSpawner, serve_connection};
use zeph_core::channel::Channel as _;

/// Minimal no-op spawner — drops the channel immediately.
fn noop_spawner() -> AgentSpawner {
    Arc::new(|channel, _ctx, _session| {
        Box::pin(async move {
            drop(channel);
        })
    })
}

/// Spawner that reads one user message then sends `Flush`, completing the turn with `EndTurn`.
fn echo_spawner() -> AgentSpawner {
    Arc::new(|mut channel, _ctx, _session| {
        Box::pin(async move {
            // Consume the user message so `do_prompt` can proceed.
            let _ = channel.recv().await;
            // Signal end of turn: drain_agent_events exits on Flush.
            let _ = channel.flush_chunks().await;
        })
    })
}

/// Like `echo_spawner`, but loops to handle multiple sequential `session/prompt` turns on the
/// same session instead of returning (and dropping the channel) after the first one.
fn multi_turn_echo_spawner() -> AgentSpawner {
    Arc::new(|mut channel, _ctx, _session| {
        Box::pin(async move {
            while let Ok(Some(_)) = channel.recv().await {
                if channel.flush_chunks().await.is_err() {
                    break;
                }
            }
        })
    })
}

/// Spawner that sends N text chunks then flushes.
fn text_chunks_spawner(chunks: Vec<&'static str>) -> AgentSpawner {
    Arc::new(move |mut channel, _ctx, _session| {
        let chunks = chunks.clone();
        Box::pin(async move {
            let _ = channel.recv().await;
            for chunk in chunks {
                let _ = channel.send_chunk(chunk).await;
            }
            let _ = channel.flush_chunks().await;
        })
    })
}

/// Spawner that requests tool-call permission via `AcpContext::permission_gate` before
/// completing the turn — reproduces the `session/request_permission` round-trip that
/// deadlocked before #6656 was fixed: the server's request-dispatch loop was blocked
/// awaiting `do_prompt` inline, so it could never route the client's permission reply
/// back to the pending `check_permission` future.
///
/// The gate's decision is forwarded on `decision_tx` so tests can assert on it directly
/// without depending on `PromptResponse` carrying assembled chunk text.
fn permission_gated_spawner(decision_tx: tokio::sync::mpsc::UnboundedSender<bool>) -> AgentSpawner {
    Arc::new(move |mut channel, ctx, session| {
        let decision_tx = decision_tx.clone();
        Box::pin(async move {
            let _ = channel.recv().await;
            let gate = ctx
                .expect("AcpContext must be present")
                .permission_gate
                .expect("permission gate must be present");
            let tool_call = acp::schema::v1::ToolCallUpdate::new(
                "tc-perm-1".to_owned(),
                acp::schema::v1::ToolCallUpdateFields::new().title("shell_execute".to_owned()),
            );
            let allowed = gate
                .check_permission(session.session_id, tool_call)
                .await
                .unwrap_or(false);
            let _ = decision_tx.send(allowed);
            let _ = channel.flush_chunks().await;
        })
    })
}

/// Minimal server config for tests.
fn test_config(name: &str) -> AcpServerConfig {
    AcpServerConfig {
        agent_name: name.to_owned(),
        agent_version: "0.0.1".to_owned(),
        max_sessions: 8,
        ..AcpServerConfig::default()
    }
}

/// Server config with a provider factory and `available_models`, required for `model` /
/// `temperature` `session/set_config_option` coverage. Every model key resolves to a fresh
/// `MockProvider`.
fn test_config_with_models(name: &str, models: Vec<&str>) -> AcpServerConfig {
    let factory: zeph_acp::ProviderFactory = Arc::new(|_key: &str| {
        Some(zeph_llm::any::AnyProvider::Mock(
            zeph_llm::mock::MockProvider::default(),
        ))
    });
    AcpServerConfig {
        agent_name: name.to_owned(),
        agent_version: "0.0.1".to_owned(),
        max_sessions: 8,
        provider_factory: Some(factory),
        available_models: Arc::new(parking_lot::RwLock::new(
            models.into_iter().map(str::to_owned).collect(),
        )),
        ..AcpServerConfig::default()
    }
}

/// Server config with provider identities for `providers/list` coverage (#5448).
#[cfg(feature = "unstable-llm-providers")]
fn test_config_with_provider_names(
    name: &str,
    providers: Vec<(&str, zeph_acp::LlmProtocol)>,
) -> AcpServerConfig {
    AcpServerConfig {
        agent_name: name.to_owned(),
        agent_version: "0.0.1".to_owned(),
        max_sessions: 8,
        provider_names: providers
            .into_iter()
            .map(|(n, p)| (n.to_owned(), p))
            .collect(),
        ..AcpServerConfig::default()
    }
}

/// Creates an in-process duplex transport pair.
/// Returns `(server_writer, server_reader, client_writer, client_reader)`.
fn duplex_pair() -> (
    impl futures::AsyncWrite + Unpin + Send + 'static,
    impl futures::AsyncRead + Unpin + Send + 'static,
    impl futures::AsyncWrite + Unpin + Send + 'static,
    impl futures::AsyncRead + Unpin + Send + 'static,
) {
    let (s_tok, c_tok) = tokio::io::duplex(64 * 1024);
    // DuplexStream implements both AsyncRead and AsyncWrite directly.
    // Use split to produce non-Clone halves that satisfy `Send + 'static`.
    let (s_read, s_write) = tokio::io::split(s_tok);
    let (c_read, c_write) = tokio::io::split(c_tok);
    (
        s_write.compat_write(),
        s_read.compat(),
        c_write.compat_write(),
        c_read.compat(),
    )
}

/// Creates a temporary working directory for tests that need a real filesystem path.
fn temp_workdir() -> TempDir {
    tempfile::tempdir().expect("failed to create temp dir")
}

/// Extracts the current selected value from a `model` / `temperature` (`Select`-kind)
/// `SessionConfigOption`. Panics if the option is not a `Select`.
fn select_current_value(option: &acp::schema::v1::SessionConfigOption) -> &str {
    match &option.kind {
        acp::schema::v1::SessionConfigKind::Select(select) => select.current_value.0.as_ref(),
        #[allow(unreachable_patterns)]
        other => panic!("expected a Select config option, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn initialize_handshake() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                let resp = cx
                    .send_request(acp::schema::v1::InitializeRequest::new(
                        acp::schema::ProtocolVersion::LATEST,
                    ))
                    .block_task()
                    .await?;
                assert!(resp.agent_info.is_some(), "agent_info missing");
                let info = resp.agent_info.unwrap();
                assert_eq!(info.name, "test-agent");
                assert_eq!(info.version, "0.0.1");
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "initialize failed: {result:?}");
                }
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn new_session_returns_session_id() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let resp = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?;

                assert!(
                    !resp.session_id.0.is_empty(),
                    "session_id must not be empty"
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "new_session failed: {result:?}");
                }
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_notification_does_not_panic() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_resp = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?;

                cx.send_notification(acp::schema::v1::CancelNotification::new(
                    session_resp.session_id,
                ))?;
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "cancel notification failed: {result:?}");
                }
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_ext_method_returns_null() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let raw_params =
                    Arc::from(serde_json::value::RawValue::from_string("{}".to_owned()).unwrap());
                let resp = cx
                    .send_request(acp::schema::v1::ClientRequest::ExtMethodRequest(
                        acp::schema::v1::ExtRequest::new("_unknown_method", raw_params),
                    ))
                    .block_task()
                    .await?;

                assert_eq!(
                    resp.to_string(),
                    "null",
                    "unknown ext method must return null"
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "ext_method failed: {result:?}");
                }
            }
        })
        .await;
}

/// #5448 regression: `providers/list` must reflect `AcpServerConfig::provider_names` as wired
/// through `build_agent_state`, not always return an empty array.
#[cfg(feature = "unstable-llm-providers")]
#[tokio::test(flavor = "current_thread")]
async fn providers_list_reflects_configured_provider_names() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (sw, sr, cw, cr) = duplex_pair();
            let config = test_config_with_provider_names(
                "test-agent",
                vec![("openai", zeph_acp::LlmProtocol::OpenAi)],
            );
            let server_fut =
                serve_connection(noop_spawner(), config, sw, sr, "acp-local".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let raw_params =
                    Arc::from(serde_json::value::RawValue::from_string("{}".to_owned()).unwrap());
                let resp = cx
                    .send_request(acp::schema::v1::ClientRequest::ExtMethodRequest(
                        acp::schema::v1::ExtRequest::new("providers/list", raw_params),
                    ))
                    .block_task()
                    .await?;

                let body = resp.to_string();
                assert!(
                    body.contains("openai"),
                    "providers/list must include the configured provider name, got: {body}"
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "providers/list ext_method failed: {result:?}");
                }
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn load_session_unknown_id_returns_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let err = cx
                    .send_request(acp::schema::v1::LoadSessionRequest::new(
                        "non-existent-session-id",
                        workdir.path(),
                    ))
                    .block_task()
                    .await;

                assert!(
                    err.is_err(),
                    "load_session of unknown id must return an error"
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "client connection failed: {result:?}");
                }
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_list_contains_created_sessions() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                // Create two sessions so the list is non-trivially non-empty.
                let id_a = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;
                let id_b = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                let resp = cx
                    .send_request(acp::schema::v1::ListSessionsRequest::new())
                    .block_task()
                    .await?;

                let ids: Vec<&acp::schema::v1::SessionId> =
                    resp.sessions.iter().map(|s| &s.session_id).collect();
                assert!(ids.contains(&&id_a), "session A not in list: {ids:?}");
                assert!(ids.contains(&&id_b), "session B not in list: {ids:?}");
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "list_sessions failed: {result:?}");
                }
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_round_trip_returns_end_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            // echo_spawner reads the message and signals Flush so drain_agent_events exits.
            let server_fut = serve_connection(
                echo_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                let content = vec![acp::schema::v1::ContentBlock::Text(
                    acp::schema::v1::TextContent::new("hello"),
                )];
                let resp = cx
                    .send_request(acp::schema::v1::PromptRequest::new(session_id, content))
                    .block_task()
                    .await?;

                assert_eq!(
                    resp.stop_reason,
                    acp::schema::v1::StopReason::EndTurn,
                    "expected EndTurn, got {:?}",
                    resp.stop_reason,
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "prompt round-trip failed: {result:?}");
                }
            }
        })
        .await;
}

/// AC #5: `drain_until_stop` collects concatenated text from multiple `AgentMessageChunk` updates.
#[tokio::test(flavor = "current_thread")]
async fn drain_until_stop_collects_text_chunks() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                text_chunks_spawner(vec!["hello", " ", "world"]),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                let content = vec![acp::schema::v1::ContentBlock::Text(
                    acp::schema::v1::TextContent::new("go"),
                )];
                let resp = cx
                    .send_request(acp::schema::v1::PromptRequest::new(session_id, content))
                    .block_task()
                    .await?;

                assert_eq!(resp.stop_reason, acp::schema::v1::StopReason::EndTurn);
                // The PromptResponse carries the assembled text from all chunks.
                // Verify the stop_reason and that the round-trip succeeded — the per-chunk
                // assembly logic is exercised by driver::drain_until_stop in client tests.
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "drain_until_stop text test failed: {result:?}");
                }
            }
        })
        .await;
}

/// AC #10: `session/cancel` prior to prompt causes the prompt to complete with
/// `StopReason::Cancelled`.
///
/// `do_cancel` stores its signal via `cancel_signal.notify_one()`. `drain_agent_events` now
/// drains any stale permit on this shared per-session `Notify` *before* its main loop starts
/// (hardening against the same leftover-permit race fixed for the `$/cancel_request` bridge —
/// see `late_cancel_after_prompt_completion_does_not_affect_next_prompt`), so a cancel that
/// arrives while no prompt is in flight on this session is a no-op rather than retroactively
/// cancelling whichever prompt happens to be sent next. `session/cancel` has no request id to
/// scope it to a specific turn, so there is no well-defined "current turn" for it to cancel
/// when none is running.
///
/// This test sends `CancelNotification` before any `PromptRequest` on a brand-new session and
/// asserts the upcoming prompt completes normally — i.e. the early cancel notification is
/// dropped, not retroactively applied.
#[tokio::test(flavor = "current_thread")]
async fn cancel_before_prompt_is_a_no_op() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                echo_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                // Cancel notification arrives with no prompt in flight on this session.
                cx.send_notification(acp::schema::v1::CancelNotification::new(session_id.clone()))?;

                let content = vec![acp::schema::v1::ContentBlock::Text(
                    acp::schema::v1::TextContent::new("go"),
                )];
                let resp = cx
                    .send_request(acp::schema::v1::PromptRequest::new(session_id, content))
                    .block_task()
                    .await?;

                assert_eq!(
                    resp.stop_reason,
                    acp::schema::v1::StopReason::EndTurn,
                    "a cancel notification sent before any prompt is in flight must not \
                     retroactively cancel the next prompt, got {:?}",
                    resp.stop_reason,
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "cancel_before_prompt test failed: {result:?}");
                }
            }
        })
        .await;
}

/// Regression test for the fixed `drain_agent_events` stale-permit race (review S-C2): a
/// cancellation that resolves once a prompt has *already finished* — e.g. a `session/cancel`
/// notification, or a late `$/cancel_request` racing the bridge in `handlers/prompt.rs` —
/// must not silently cancel the next, unrelated prompt on the same session via a leftover
/// permit on the shared `cancel_signal: Arc<Notify>`.
///
/// Simulated deterministically via the public `CancelNotification` protocol message (which
/// notifies the very same `cancel_signal` `do_cancel` and the `$/cancel_request` bridge both
/// use) sent strictly *after* the first prompt's response has already been received.
#[tokio::test(flavor = "current_thread")]
async fn late_cancel_after_prompt_completion_does_not_affect_next_prompt() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                multi_turn_echo_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                let first_content = vec![acp::schema::v1::ContentBlock::Text(
                    acp::schema::v1::TextContent::new("first"),
                )];
                let first = cx
                    .send_request(acp::schema::v1::PromptRequest::new(
                        session_id.clone(),
                        first_content,
                    ))
                    .block_task()
                    .await?;
                assert_eq!(
                    first.stop_reason,
                    acp::schema::v1::StopReason::EndTurn,
                    "first prompt must complete normally before the late cancel arrives"
                );

                // No prompt is in flight at this point — this notify leaves a permit on the
                // shared `cancel_signal` that must be drained before the next prompt's
                // `drain_agent_events` loop starts.
                cx.send_notification(acp::schema::v1::CancelNotification::new(session_id.clone()))?;

                let second_content = vec![acp::schema::v1::ContentBlock::Text(
                    acp::schema::v1::TextContent::new("second"),
                )];
                let second = cx
                    .send_request(acp::schema::v1::PromptRequest::new(
                        session_id,
                        second_content,
                    ))
                    .block_task()
                    .await?;
                assert_eq!(
                    second.stop_reason,
                    acp::schema::v1::StopReason::EndTurn,
                    "a cancel notification that arrives between two prompts must not cancel \
                     the next, unrelated prompt, got {:?}",
                    second.stop_reason,
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "late cancel regression test failed: {result:?}");
                }
            }
        })
        .await;
}

/// `authenticate` is a no-op (vault-based auth) but must round-trip successfully (#5367).
#[tokio::test(flavor = "current_thread")]
async fn authenticate_returns_default_response() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                cx.send_request(acp::schema::v1::AuthenticateRequest::new("agent"))
                    .block_task()
                    .await?;
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "authenticate failed: {result:?}");
                }
            }
        })
        .await;
}

/// `logout` is a no-op (vault-based auth) but must round-trip successfully (#5367).
#[tokio::test(flavor = "current_thread")]
async fn logout_returns_default_response() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                cx.send_request(acp::schema::v1::LogoutRequest::new())
                    .block_task()
                    .await?;
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "logout failed: {result:?}");
                }
            }
        })
        .await;
}

/// `session/close` flushes and removes the session: a subsequent `session/load` for the same
/// id must fail since no store is configured and the in-memory entry is gone (#5367).
#[tokio::test(flavor = "current_thread")]
async fn close_session_removes_session_from_memory() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                cx.send_request(acp::schema::v1::CloseSessionRequest::new(
                    session_id.clone(),
                ))
                .block_task()
                .await?;

                let load_err = cx
                    .send_request(acp::schema::v1::LoadSessionRequest::new(
                        session_id,
                        workdir.path(),
                    ))
                    .block_task()
                    .await;
                assert!(
                    load_err.is_err(),
                    "loading a closed session must fail when no store is configured"
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "close_session test failed: {result:?}");
                }
            }
        })
        .await;
}

/// `session/delete` removes the session from `session/list` (#5367) and, when a store is
/// configured, permanently removes the persisted row too — a deleted session must never
/// resurrect via `session/load`/`session/resume` (#6271).
#[tokio::test(flavor = "current_thread")]
async fn delete_session_removes_session_from_list() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let db_dir = tempfile::tempdir().expect("failed to create temp db dir");
            let sqlite_path = db_dir
                .path()
                .join("acp-delete-test.db")
                .to_string_lossy()
                .into_owned();
            let (sw, sr, cw, cr) = duplex_pair();
            let config = AcpServerConfig {
                sqlite_path: Some(sqlite_path.clone()),
                ..test_config("test-agent")
            };
            let server_fut =
                serve_connection(noop_spawner(), config, sw, sr, "acp-local".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                cx.send_request(acp::schema::v1::DeleteSessionRequest::new(
                    session_id.clone(),
                ))
                .block_task()
                .await?;

                let resp = cx
                    .send_request(acp::schema::v1::ListSessionsRequest::new())
                    .block_task()
                    .await?;
                let ids: Vec<&acp::schema::v1::SessionId> =
                    resp.sessions.iter().map(|s| &s.session_id).collect();
                assert!(
                    !ids.contains(&&session_id),
                    "deleted session must not appear in session/list: {ids:?}"
                );
                Ok(session_id)
            });
            let session_id = tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    result.expect("delete_session test failed")
                }
            };

            // Verify the row is actually gone from the persistence store, not just absent
            // from the in-memory session/list — the regression this test guards against
            // (#6271) is a deleted session resurrecting via load/resume because the store
            // row survived.
            let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                .await
                .expect("SqliteStore::new");
            assert!(
                !store
                    .acp_session_exists(&session_id.to_string())
                    .await
                    .expect("acp_session_exists query failed"),
                "deleted session must not survive in the persistence store"
            );
        })
        .await;
}

/// `session/fork` creates a new session with a distinct id from the source (#5367).
#[cfg(feature = "unstable-session-fork")]
#[tokio::test(flavor = "current_thread")]
async fn fork_session_creates_distinct_session_id() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let source_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                let forked = cx
                    .send_request(acp::schema::v1::ForkSessionRequest::new(
                        source_id.clone(),
                        workdir.path(),
                    ))
                    .block_task()
                    .await?;

                assert_ne!(
                    forked.session_id, source_id,
                    "forked session must have a distinct id from the source"
                );
                assert!(!forked.session_id.0.is_empty());
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "fork_session test failed: {result:?}");
                }
            }
        })
        .await;
}

/// `session/fork` copies the source session's durable JSONL event log into a new,
/// self-contained child log when `[session] enabled = true` (spec-068 P2, #5343).
///
/// The mock spawner used by this test harness never runs a real `zeph_core::Agent` (that only
/// happens in the root binary's `spawn_acp_agent`), so it cannot generate turns through the
/// normal `SessionSink` path. Instead this test seeds the source session's `events.jsonl`
/// directly via `zeph_session::SessionEventLog`, exercising exactly the `fork_conversation` /
/// `ForkEngine::fork` wiring under test without needing the full agent-loop integration.
#[cfg(feature = "unstable-session-fork")]
#[tokio::test(flavor = "current_thread")]
async fn fork_session_copies_event_log_when_persistence_enabled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let db_dir = tempfile::tempdir().expect("failed to create temp db dir");
            let sqlite_path = db_dir
                .path()
                .join("acp-fork-test.db")
                .to_string_lossy()
                .into_owned();
            let session_data_dir = db_dir.path().join("sessions");

            let (sw, sr, cw, cr) = duplex_pair();
            let config = AcpServerConfig {
                sqlite_path: Some(sqlite_path),
                session_data_dir: Some(session_data_dir.clone()),
                ..test_config("test-agent")
            };
            let server_fut =
                serve_connection(noop_spawner(), config, sw, sr, "acp-local".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let source_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                // Seed the source session's event log directly (simulating turns that a real
                // agent loop would have appended via SessionSink).
                let source_dir =
                    zeph_session::session_dir(&session_data_dir, &source_id.to_string());
                let log = zeph_session::SessionEventLog::open(&source_dir)
                    .await
                    .expect("open source event log");
                log.append(
                    None,
                    None,
                    zeph_session::SessionEvent::SessionStarted {
                        session_id: source_id.to_string(),
                        cwd: workdir.path().to_string_lossy().into_owned(),
                        provider_name: "claude".to_owned(),
                        model: "opus".to_owned(),
                        forked_from: None,
                    },
                )
                .await
                .expect("append SessionStarted");
                log.append(
                    None,
                    None,
                    zeph_session::SessionEvent::UserMessage {
                        text: "hello".to_owned(),
                        image_refs: vec![],
                    },
                )
                .await
                .expect("append UserMessage");

                let forked = cx
                    .send_request(acp::schema::v1::ForkSessionRequest::new(
                        source_id.clone(),
                        workdir.path(),
                    ))
                    .block_task()
                    .await?;

                let child_dir =
                    zeph_session::session_dir(&session_data_dir, &forked.session_id.to_string());
                let child_log = zeph_session::SessionEventLog::open(&child_dir)
                    .await
                    .expect("open child event log");
                let events = child_log.read_all().await.expect("read child event log");
                // 1 synthesized SessionStarted header (forked_from) + the 2 seeded events.
                assert_eq!(events.len(), 3, "child log must contain the copied events");
                assert!(matches!(
                    events[0].kind,
                    zeph_session::SessionEvent::SessionStarted {
                        forked_from: Some(_),
                        ..
                    }
                ));

                let parent_log = zeph_session::SessionEventLog::open(&source_dir)
                    .await
                    .expect("reopen source event log");
                let parent_events = parent_log.read_all().await.expect("read source event log");
                assert!(
                    matches!(
                        parent_events.last().expect("parent has events").kind,
                        zeph_session::SessionEvent::ForkPoint { .. }
                    ),
                    "parent log must record a ForkPoint provenance event"
                );

                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "fork_session persistence test failed: {result:?}");
                }
            }
        })
        .await;
}

/// `session/resume` reconnects to an in-memory session by id (#5367).
#[tokio::test(flavor = "current_thread")]
async fn resume_session_reconnects_to_existing_session() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                cx.send_request(acp::schema::v1::ResumeSessionRequest::new(
                    session_id,
                    workdir.path(),
                ))
                .block_task()
                .await?;
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "resume_session test failed: {result:?}");
                }
            }
        })
        .await;
}

/// `session/resume` reconstructs a session from the `SQLite` store when it is no longer
/// held in the agent's in-memory `sessions` map — the scenario that matters in practice,
/// e.g. reconnecting after a server restart (#5374).
///
/// Two independent `serve_connection` calls share the same `sqlite_path`, so the second
/// connection gets a brand-new `ZephAcpAgentState` with an empty `sessions` map: any
/// successful `session/resume` there can only come from the store-backed reconstruction
/// branch (`store.acp_session_exists` + `resolve_conversation_id` + `make_session_entry` +
/// `spawn_local` re-attach), not the in-memory early-return checked by
/// `resume_session_reconnects_to_existing_session`. A follow-up `session/prompt` on the
/// resumed session proves the reconstructed entry is actually wired up and functional.
#[tokio::test(flavor = "current_thread")]
async fn resume_session_reconstructs_from_store_after_restart() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let db_dir = tempfile::tempdir().expect("failed to create temp db dir");
            let sqlite_path = db_dir
                .path()
                .join("acp-resume-test.db")
                .to_string_lossy()
                .into_owned();

            // First connection: create the session, then drop the connection (and its
            // in-memory ZephAcpAgentState) without ever calling session/resume on it.
            let session_id = {
                let (sw, sr, cw, cr) = duplex_pair();
                let config = AcpServerConfig {
                    sqlite_path: Some(sqlite_path.clone()),
                    ..test_config("test-agent")
                };
                let server_fut =
                    serve_connection(noop_spawner(), config, sw, sr, "acp-local".to_owned());
                let client_fut =
                    acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                        cx.send_request(acp::schema::v1::InitializeRequest::new(
                            acp::schema::ProtocolVersion::LATEST,
                        ))
                        .block_task()
                        .await?;

                        let session_id = cx
                            .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                            .block_task()
                            .await?
                            .session_id;
                        Ok(session_id)
                    });
                tokio::select! {
                    res = server_fut => panic!("server exited before client: {res:?}"),
                    result = client_fut => result.expect("session/new failed"),
                }
            };

            // Second connection: fresh ZephAcpAgentState (empty `sessions` map), same
            // sqlite_path. `session/resume` here can only succeed via the store-backed path.
            let (sw, sr, cw, cr) = duplex_pair();
            let config = AcpServerConfig {
                sqlite_path: Some(sqlite_path.clone()),
                ..test_config("test-agent")
            };
            let server_fut =
                serve_connection(echo_spawner(), config, sw, sr, "acp-local".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                cx.send_request(acp::schema::v1::ResumeSessionRequest::new(
                    session_id.clone(),
                    workdir.path(),
                ))
                .block_task()
                .await?;

                // Prove the rebuilt session is actually functional: a real agent-loop task
                // must have been spawned and wired to the reconstructed channel entry.
                let content = vec![acp::schema::v1::ContentBlock::Text(
                    acp::schema::v1::TextContent::new("hello again"),
                )];
                let resp = cx
                    .send_request(acp::schema::v1::PromptRequest::new(session_id, content))
                    .block_task()
                    .await?;
                assert_eq!(
                    resp.stop_reason,
                    acp::schema::v1::StopReason::EndTurn,
                    "expected EndTurn from the store-reconstructed session, got {:?}",
                    resp.stop_reason,
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(
                        result.is_ok(),
                        "store-backed resume_session test failed: {result:?}"
                    );
                }
            }
        })
        .await;
}

/// `session/load` replays from the durable JSONL event log (spec-068 §12.3 / D-2), not the
/// legacy `acp_session_events` table, which the P1 write cutover leaves permanently empty for
/// post-cutover sessions (S1 regression fix). This test seeds only the JSONL log — never
/// `save_acp_event` — so a `session/load` that still reached into the legacy table would find
/// nothing there while this test's event still proves the store-backed reconstruction branch
/// (fresh connection, empty in-memory `sessions` map) can load the session at all.
#[tokio::test(flavor = "current_thread")]
async fn load_session_succeeds_from_event_log_with_no_legacy_rows() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let db_dir = tempfile::tempdir().expect("failed to create temp db dir");
            let sqlite_path = db_dir
                .path()
                .join("acp-load-test.db")
                .to_string_lossy()
                .into_owned();
            let session_data_dir = db_dir.path().join("sessions");

            let session_id = {
                let (sw, sr, cw, cr) = duplex_pair();
                let config = AcpServerConfig {
                    sqlite_path: Some(sqlite_path.clone()),
                    session_data_dir: Some(session_data_dir.clone()),
                    ..test_config("test-agent")
                };
                let server_fut =
                    serve_connection(noop_spawner(), config, sw, sr, "acp-local".to_owned());
                let client_fut =
                    acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                        cx.send_request(acp::schema::v1::InitializeRequest::new(
                            acp::schema::ProtocolVersion::LATEST,
                        ))
                        .block_task()
                        .await?;

                        let session_id = cx
                            .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                            .block_task()
                            .await?
                            .session_id;
                        Ok(session_id)
                    });
                tokio::select! {
                    res = server_fut => panic!("server exited before client: {res:?}"),
                    result = client_fut => result.expect("session/new failed"),
                }
            };

            // Seed only the durable JSONL log (as SessionSink would in production) — no
            // acp_session_events legacy rows exist for this session at all.
            let session_dir = zeph_session::session_dir(&session_data_dir, &session_id.to_string());
            let log = zeph_session::SessionEventLog::open(&session_dir)
                .await
                .expect("open event log");
            log.append(
                None,
                None,
                zeph_session::SessionEvent::UserMessage {
                    text: "hello".to_owned(),
                    image_refs: vec![],
                },
            )
            .await
            .expect("append UserMessage");

            // Fresh connection: empty in-memory `sessions` map, so `session/load` can only
            // succeed via the store-backed reconstruction branch.
            let (sw, sr, cw, cr) = duplex_pair();
            let config = AcpServerConfig {
                sqlite_path: Some(sqlite_path),
                session_data_dir: Some(session_data_dir),
                ..test_config("test-agent")
            };
            let server_fut =
                serve_connection(noop_spawner(), config, sw, sr, "acp-local".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                cx.send_request(acp::schema::v1::LoadSessionRequest::new(
                    session_id,
                    workdir.path(),
                ))
                .block_task()
                .await?;
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "session/load from event log failed: {result:?}");
                }
            }
        })
        .await;
}

/// `session/fork` inherits the source session's current `temperature`, `thinking`, and
/// `auto_approve` config from its live in-memory state, rather than resetting to configured
/// defaults (#5373).
#[cfg(feature = "unstable-session-fork")]
#[tokio::test(flavor = "current_thread")]
async fn fork_session_inherits_config_from_in_memory_source() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            // Configured default is "balanced" — the source session will be switched away
            // from it so the test can distinguish "inherited" from "reset to default".
            let server_fut = serve_connection(
                noop_spawner(),
                test_config_with_models("test-agent", vec!["claude:sonnet"]),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let source_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                cx.send_request(acp::schema::v1::SetSessionConfigOptionRequest::new(
                    source_id.clone(),
                    "temperature",
                    "creative",
                ))
                .block_task()
                .await?;
                cx.send_request(acp::schema::v1::SetSessionConfigOptionRequest::new(
                    source_id.clone(),
                    "thinking",
                    "on",
                ))
                .block_task()
                .await?;
                cx.send_request(acp::schema::v1::SetSessionConfigOptionRequest::new(
                    source_id.clone(),
                    "auto_approve",
                    "auto-edit",
                ))
                .block_task()
                .await?;

                let forked = cx
                    .send_request(acp::schema::v1::ForkSessionRequest::new(
                        source_id,
                        workdir.path(),
                    ))
                    .block_task()
                    .await?;

                let options = forked.config_options.unwrap_or_default();
                let get = |id: &str| {
                    select_current_value(options.iter().find(|o| o.id.0.as_ref() == id).unwrap())
                        .to_owned()
                };
                assert_eq!(
                    get("temperature"),
                    "creative",
                    "forked session must inherit the source's temperature preset, not reset to \
                     the configured default"
                );
                assert_eq!(
                    get("thinking"),
                    "on",
                    "forked session must inherit the source's thinking toggle"
                );
                assert_eq!(
                    get("auto_approve"),
                    "auto-edit",
                    "forked session must inherit the source's auto-approve level"
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "fork inheritance test failed: {result:?}");
                }
            }
        })
        .await;
}

/// `session/resume` of a session that was gracefully closed inherits its persisted config
/// snapshot (temperature preset applied to the effective provider) rather than resetting to
/// the configured default (#5373).
#[tokio::test(flavor = "current_thread")]
async fn resume_session_inherits_temperature_preset_after_close() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();

            let captured: Arc<std::sync::Mutex<Option<zeph_llm::provider::GenerationOverrides>>> =
                Arc::new(std::sync::Mutex::new(None));
            let captured_for_factory = Arc::clone(&captured);
            let factory: zeph_acp::ProviderFactory = Arc::new(move |_key: &str| {
                Some(zeph_llm::any::AnyProvider::Mock(
                    zeph_llm::mock::MockProvider::default()
                        .with_overrides_capture(Arc::clone(&captured_for_factory)),
                ))
            });

            let mut config = test_config_with_models("test-agent", vec!["claude:sonnet"]);
            config.provider_factory = Some(factory);
            config.sqlite_path = Some(":memory:".to_owned());

            let server_fut =
                serve_connection(noop_spawner(), config, sw, sr, "acp-local".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                // Switch away from the configured default ("balanced") before closing.
                cx.send_request(acp::schema::v1::SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    "temperature",
                    "creative",
                ))
                .block_task()
                .await?;

                cx.send_request(acp::schema::v1::CloseSessionRequest::new(
                    session_id.clone(),
                ))
                .block_task()
                .await?;

                cx.send_request(acp::schema::v1::ResumeSessionRequest::new(
                    session_id,
                    workdir.path(),
                ))
                .block_task()
                .await?;
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "resume inheritance test failed: {result:?}");
                }
            }

            let applied_temperature = captured
                .lock()
                .expect("capture mutex poisoned")
                .as_ref()
                .and_then(|o| o.temperature);
            assert_eq!(
                applied_temperature,
                Some(zeph_config::AcpTemperaturePreset::Creative.temperature()),
                "resumed session must inherit the closed source's persisted temperature preset, \
                 not reset to the configured default"
            );
        })
        .await;
}

/// `session/set_mode` switches the active mode and is reflected in subsequent requests (#5367).
#[tokio::test(flavor = "current_thread")]
async fn set_session_mode_switches_mode() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                cx.send_request(acp::schema::v1::SetSessionModeRequest::new(
                    session_id,
                    "architect",
                ))
                .block_task()
                .await?;
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "set_session_mode test failed: {result:?}");
                }
            }
        })
        .await;
}

/// `session/set_config_option` with `config_id="model"` switches the active model and echoes
/// it back in `config_options` (#5367 coverage; pre-existing handler, previously untested).
#[tokio::test(flavor = "current_thread")]
async fn set_session_config_option_model_switches_active_model() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config_with_models("test-agent", vec!["claude:sonnet", "ollama:llama3"]),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                let resp = cx
                    .send_request(acp::schema::v1::SetSessionConfigOptionRequest::new(
                        session_id,
                        "model",
                        "ollama:llama3",
                    ))
                    .block_task()
                    .await?;

                let model_option = resp
                    .config_options
                    .iter()
                    .find(|o| o.id.0.as_ref() == "model")
                    .expect("model option must be present");
                assert_eq!(
                    select_current_value(model_option),
                    "ollama:llama3",
                    "model option must reflect the switched model"
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "set_config_option model test failed: {result:?}");
                }
            }
        })
        .await;
}

/// `session/set_config_option` with `config_id="temperature"` (`model_config` category, #5361)
/// switches the sampling-temperature preset and echoes it back in `config_options`.
#[tokio::test(flavor = "current_thread")]
async fn set_session_config_option_temperature_preset_changes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config_with_models("test-agent", vec!["claude:sonnet"]),
                sw,
                sr,
            "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_resp = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?;
                let session_id = session_resp.session_id;

                // The default preset ("balanced") must already be advertised on session creation.
                let initial_temperature = session_resp
                    .config_options
                    .unwrap_or_default()
                    .into_iter()
                    .find(|o| o.id.0.as_ref() == "temperature")
                    .expect("temperature option must be advertised in new_session response");
                assert_eq!(select_current_value(&initial_temperature), "balanced");

                let resp = cx
                    .send_request(acp::schema::v1::SetSessionConfigOptionRequest::new(
                        session_id,
                        "temperature",
                        "creative",
                    ))
                    .block_task()
                    .await?;

                let temperature_option = resp
                    .config_options
                    .iter()
                    .find(|o| o.id.0.as_ref() == "temperature")
                    .expect("temperature option must be present");
                assert_eq!(
                    select_current_value(temperature_option),
                    "creative",
                    "temperature option must reflect the switched preset"
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "set_config_option temperature test failed: {result:?}");
                }
            }
        })
        .await;
}

/// Regression test for review finding S-C1: `[acp.model_config].default_temperature_preset`
/// must be primed into the session's *effective* provider at session creation
/// (`prime_provider_override` in `agent/mod.rs`) — not just advertised as the `temperature`
/// config option's current value in the IDE dropdown — even when no
/// `session/set_config_option` call is ever made.
///
/// Verified via `MockProvider::with_overrides_capture`: the test-only `ProviderFactory` shares
/// one capture slot across every provider it builds, so it observes whatever
/// `GenerationOverrides` production code applied internally during `new_session`.
#[tokio::test(flavor = "current_thread")]
async fn default_temperature_preset_is_primed_at_session_creation() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();

            let captured: Arc<std::sync::Mutex<Option<zeph_llm::provider::GenerationOverrides>>> =
                Arc::new(std::sync::Mutex::new(None));
            let captured_for_factory = Arc::clone(&captured);
            let factory: zeph_acp::ProviderFactory = Arc::new(move |_key: &str| {
                Some(zeph_llm::any::AnyProvider::Mock(
                    zeph_llm::mock::MockProvider::default()
                        .with_overrides_capture(Arc::clone(&captured_for_factory)),
                ))
            });

            let mut config = test_config_with_models("test-agent", vec!["claude:sonnet"]);
            config.provider_factory = Some(factory);
            config.model_config = zeph_config::AcpModelConfigConfig {
                default_temperature_preset: zeph_config::AcpTemperaturePreset::Creative,
            };

            let server_fut =
                serve_connection(noop_spawner(), config, sw, sr, "acp-local".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                // No session/set_config_option call is made — the default preset must already
                // be effective from session creation alone.
                cx.send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?;
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "client connection failed: {result:?}");
                }
            }

            let applied_temperature = captured
                .lock()
                .expect("capture mutex poisoned")
                .as_ref()
                .and_then(|o| o.temperature);
            assert_eq!(
                applied_temperature,
                Some(zeph_config::AcpTemperaturePreset::Creative.temperature()),
                "default_temperature_preset must be primed into the effective provider at \
                 session creation, with no session/set_config_option call made"
            );
        })
        .await;
}

/// `session/set_config_option` with an unrecognized `config_id` must error, not silently
/// succeed (#5367 coverage).
#[tokio::test(flavor = "current_thread")]
async fn set_session_config_option_unknown_config_id_errors() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let server_fut = serve_connection(
                noop_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                let err = cx
                    .send_request(acp::schema::v1::SetSessionConfigOptionRequest::new(
                        session_id,
                        "nonexistent_option",
                        "whatever",
                    ))
                    .block_task()
                    .await;
                assert!(err.is_err(), "unknown config_id must return an error");
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "client connection failed: {result:?}");
                }
            }
        })
        .await;
}

/// The real `$/cancel_request` protocol notification (#5362), sent for the in-flight
/// `session/prompt` JSON-RPC request, cancels the prompt the same way `session/cancel` does.
#[cfg(feature = "unstable-cancel-request")]
#[tokio::test(flavor = "current_thread")]
async fn cancel_request_during_prompt_cancels() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            // echo_spawner reads the message and flushes; the $/cancel_request watcher in
            // handle_prompt notifies the same cancel_signal session/cancel uses.
            let server_fut = serve_connection(
                echo_spawner(),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;

                let session_id = cx
                    .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                    .block_task()
                    .await?
                    .session_id;

                let content = vec![acp::schema::v1::ContentBlock::Text(
                    acp::schema::v1::TextContent::new("go"),
                )];
                let request =
                    cx.send_request(acp::schema::v1::PromptRequest::new(session_id, content));
                // Cancel the specific in-flight session/prompt JSON-RPC request via the real
                // protocol-level $/cancel_request notification (distinct from session/cancel).
                request.cancel()?;

                let resp = request.block_task().await;
                // Cooperative cancellation: the handler may still finish with EndTurn if the
                // watcher loses the race, or return the standard cancellation error, or
                // (when the cancel_signal wins inside drain_agent_events) complete with
                // StopReason::Cancelled. All three are valid SDK-documented outcomes; the
                // assertion only rules out a hang or panic.
                match resp {
                    Ok(r) => {
                        assert!(matches!(
                            r.stop_reason,
                            acp::schema::v1::StopReason::EndTurn
                                | acp::schema::v1::StopReason::Cancelled
                        ));
                    }
                    Err(e) => {
                        assert_eq!(
                            i32::from(e.code),
                            -32800,
                            "expected request_cancelled error"
                        );
                    }
                }
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "cancel_request test failed: {result:?}");
                }
            }
        })
        .await;
}

// ── Cross-owner scoping at the stdio JSON-RPC layer (#5868) ──────────────────────────

/// Creates a fresh connection with the given `owner`, issues `session/new`, and disconnects.
/// Helper for the cross-owner tests below — a shared `sqlite_path` means the resulting session
/// persists with `owner` stamped as its `owner_key`.
async fn create_owned_session(
    sqlite_path: &str,
    owner: &str,
    workdir: &std::path::Path,
) -> acp::schema::v1::SessionId {
    let (sw, sr, cw, cr) = duplex_pair();
    let config = AcpServerConfig {
        sqlite_path: Some(sqlite_path.to_owned()),
        ..test_config("test-agent")
    };
    let server_fut = serve_connection(noop_spawner(), config, sw, sr, owner.to_owned());
    let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
        cx.send_request(acp::schema::v1::InitializeRequest::new(
            acp::schema::ProtocolVersion::LATEST,
        ))
        .block_task()
        .await?;
        let session_id = cx
            .send_request(acp::schema::v1::NewSessionRequest::new(workdir))
            .block_task()
            .await?
            .session_id;
        Ok(session_id)
    });
    tokio::select! {
        res = server_fut => panic!("server exited before client: {res:?}"),
        result = client_fut => result.expect("session/new failed"),
    }
}

/// `session/list` scopes persisted sessions to the calling connection's `owner_key`: a session
/// created by one connection's owner must not appear in a different owner's list, even when
/// both connections share the same `sqlite_path`.
#[tokio::test(flavor = "current_thread")]
async fn list_sessions_isolated_by_owner_across_stdio_connections() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let db_dir = tempfile::tempdir().expect("failed to create temp db dir");
            let sqlite_path = db_dir
                .path()
                .join("acp-owner-list-test.db")
                .to_string_lossy()
                .into_owned();

            // Two distinct owners each create a session on the shared store, then disconnect.
            let alice_session = create_owned_session(&sqlite_path, "alice", workdir.path()).await;
            let bob_session = create_owned_session(&sqlite_path, "bob", workdir.path()).await;

            // A fresh connection as "alice" lists sessions: must see her own, not bob's.
            let (sw, sr, cw, cr) = duplex_pair();
            let config = AcpServerConfig {
                sqlite_path: Some(sqlite_path.clone()),
                ..test_config("test-agent")
            };
            let server_fut = serve_connection(noop_spawner(), config, sw, sr, "alice".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;
                let resp = cx
                    .send_request(acp::schema::v1::ListSessionsRequest::new())
                    .block_task()
                    .await?;
                let ids: Vec<&acp::schema::v1::SessionId> =
                    resp.sessions.iter().map(|s| &s.session_id).collect();
                assert!(
                    ids.contains(&&alice_session),
                    "alice's own session missing from her list: {ids:?}"
                );
                assert!(
                    !ids.contains(&&bob_session),
                    "bob's session leaked into alice's list: {ids:?}"
                );
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(result.is_ok(), "cross-owner list_sessions test failed: {result:?}");
                }
            }
        })
        .await;
}

/// `session/resume` on a session owned by a different connection must fail — a foreign
/// `owner_key` is indistinguishable from a nonexistent session id.
#[tokio::test(flavor = "current_thread")]
async fn resume_session_cross_owner_fails() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let db_dir = tempfile::tempdir().expect("failed to create temp db dir");
            let sqlite_path = db_dir
                .path()
                .join("acp-owner-resume-test.db")
                .to_string_lossy()
                .into_owned();

            let session_id = {
                let (sw, sr, cw, cr) = duplex_pair();
                let config = AcpServerConfig {
                    sqlite_path: Some(sqlite_path.clone()),
                    ..test_config("test-agent")
                };
                let server_fut =
                    serve_connection(noop_spawner(), config, sw, sr, "alice".to_owned());
                let client_fut =
                    acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                        cx.send_request(acp::schema::v1::InitializeRequest::new(
                            acp::schema::ProtocolVersion::LATEST,
                        ))
                        .block_task()
                        .await?;
                        let session_id = cx
                            .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                            .block_task()
                            .await?
                            .session_id;
                        Ok(session_id)
                    });
                tokio::select! {
                    res = server_fut => panic!("server exited before client: {res:?}"),
                    result = client_fut => result.expect("session/new failed"),
                }
            };

            // A different owner ("bob") tries to resume alice's session on the same store.
            let (sw, sr, cw, cr) = duplex_pair();
            let config = AcpServerConfig {
                sqlite_path: Some(sqlite_path.clone()),
                ..test_config("test-agent")
            };
            let server_fut = serve_connection(echo_spawner(), config, sw, sr, "bob".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;
                cx.send_request(acp::schema::v1::ResumeSessionRequest::new(
                    session_id,
                    workdir.path(),
                ))
                .block_task()
                .await
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(
                        result.is_err(),
                        "bob must not be able to resume alice's session, got: {result:?}"
                    );
                }
            }
        })
        .await;
}

/// `session/load` on a session owned by a different connection must fail (mirrors
/// `resume_session_cross_owner_fails` for the `load_session` handler).
#[tokio::test(flavor = "current_thread")]
async fn load_session_cross_owner_fails() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let db_dir = tempfile::tempdir().expect("failed to create temp db dir");
            let sqlite_path = db_dir
                .path()
                .join("acp-owner-load-test.db")
                .to_string_lossy()
                .into_owned();

            let session_id = {
                let (sw, sr, cw, cr) = duplex_pair();
                let config = AcpServerConfig {
                    sqlite_path: Some(sqlite_path.clone()),
                    ..test_config("test-agent")
                };
                let server_fut =
                    serve_connection(noop_spawner(), config, sw, sr, "alice".to_owned());
                let client_fut =
                    acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                        cx.send_request(acp::schema::v1::InitializeRequest::new(
                            acp::schema::ProtocolVersion::LATEST,
                        ))
                        .block_task()
                        .await?;
                        let session_id = cx
                            .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                            .block_task()
                            .await?
                            .session_id;
                        Ok(session_id)
                    });
                tokio::select! {
                    res = server_fut => panic!("server exited before client: {res:?}"),
                    result = client_fut => result.expect("session/new failed"),
                }
            };

            let (sw, sr, cw, cr) = duplex_pair();
            let config = AcpServerConfig {
                sqlite_path: Some(sqlite_path.clone()),
                ..test_config("test-agent")
            };
            let server_fut = serve_connection(echo_spawner(), config, sw, sr, "bob".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;
                cx.send_request(acp::schema::v1::LoadSessionRequest::new(
                    session_id,
                    workdir.path(),
                ))
                .block_task()
                .await
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(
                        result.is_err(),
                        "bob must not be able to load alice's session, got: {result:?}"
                    );
                }
            }
        })
        .await;
}

/// `session/delete` on a session owned by a different connection must not remove the
/// persisted row — mirrors `load_session_cross_owner_fails`/`resume_session_cross_owner_fails`
/// but for the delete path, at the ACP-protocol handler level (the underlying
/// `delete_acp_session_for_owner` SQL owner-scoping is already unit-tested in
/// `zeph-memory`; the HTTP CRUD transport has its own
/// `delete_session_cross_owner_returns_404_and_does_not_delete` — this is the missing
/// coverage for the `do_delete_session` ACP handler itself) (#6271).
#[tokio::test(flavor = "current_thread")]
async fn delete_session_cross_owner_fails() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let db_dir = tempfile::tempdir().expect("failed to create temp db dir");
            let sqlite_path = db_dir
                .path()
                .join("acp-owner-delete-test.db")
                .to_string_lossy()
                .into_owned();

            let session_id = {
                let (sw, sr, cw, cr) = duplex_pair();
                let config = AcpServerConfig {
                    sqlite_path: Some(sqlite_path.clone()),
                    ..test_config("test-agent")
                };
                let server_fut =
                    serve_connection(noop_spawner(), config, sw, sr, "alice".to_owned());
                let client_fut =
                    acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                        cx.send_request(acp::schema::v1::InitializeRequest::new(
                            acp::schema::ProtocolVersion::LATEST,
                        ))
                        .block_task()
                        .await?;
                        let session_id = cx
                            .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                            .block_task()
                            .await?
                            .session_id;
                        Ok(session_id)
                    });
                tokio::select! {
                    res = server_fut => panic!("server exited before client: {res:?}"),
                    result = client_fut => result.expect("session/new failed"),
                }
            };

            let (sw, sr, cw, cr) = duplex_pair();
            let config = AcpServerConfig {
                sqlite_path: Some(sqlite_path.clone()),
                ..test_config("test-agent")
            };
            let server_fut = serve_connection(noop_spawner(), config, sw, sr, "bob".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;
                // Bob's request may return Ok (delete is a no-op for a foreign/nonexistent id,
                // same uniform non-distinguishing shape as `claim_acp_session_for_owner`) or
                // Err — either is acceptable here; what matters is verified below: alice's row
                // must survive.
                let _ = cx
                    .send_request(acp::schema::v1::DeleteSessionRequest::new(
                        session_id.clone(),
                    ))
                    .block_task()
                    .await;
                Ok(())
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    result.expect("bob's delete_session request errored unexpectedly");
                }
            }

            let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                .await
                .expect("SqliteStore::new");
            assert!(
                store
                    .acp_session_exists(&session_id.to_string())
                    .await
                    .expect("acp_session_exists query failed"),
                "bob must not be able to delete alice's session from the store"
            );
        })
        .await;
}

/// `session/fork` sourced from a session owned by a different connection must fail.
#[cfg(feature = "unstable-session-fork")]
#[tokio::test(flavor = "current_thread")]
async fn fork_session_cross_owner_fails() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let db_dir = tempfile::tempdir().expect("failed to create temp db dir");
            let sqlite_path = db_dir
                .path()
                .join("acp-owner-fork-test.db")
                .to_string_lossy()
                .into_owned();

            let session_id = {
                let (sw, sr, cw, cr) = duplex_pair();
                let config = AcpServerConfig {
                    sqlite_path: Some(sqlite_path.clone()),
                    ..test_config("test-agent")
                };
                let server_fut =
                    serve_connection(noop_spawner(), config, sw, sr, "alice".to_owned());
                let client_fut =
                    acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                        cx.send_request(acp::schema::v1::InitializeRequest::new(
                            acp::schema::ProtocolVersion::LATEST,
                        ))
                        .block_task()
                        .await?;
                        let session_id = cx
                            .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                            .block_task()
                            .await?
                            .session_id;
                        Ok(session_id)
                    });
                tokio::select! {
                    res = server_fut => panic!("server exited before client: {res:?}"),
                    result = client_fut => result.expect("session/new failed"),
                }
            };

            let (sw, sr, cw, cr) = duplex_pair();
            let config = AcpServerConfig {
                sqlite_path: Some(sqlite_path.clone()),
                ..test_config("test-agent")
            };
            let server_fut = serve_connection(noop_spawner(), config, sw, sr, "bob".to_owned());
            let client_fut = acp::Client.connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                cx.send_request(acp::schema::v1::InitializeRequest::new(
                    acp::schema::ProtocolVersion::LATEST,
                ))
                .block_task()
                .await?;
                cx.send_request(acp::schema::v1::ForkSessionRequest::new(
                    session_id,
                    workdir.path(),
                ))
                .block_task()
                .await
            });
            tokio::select! {
                res = server_fut => panic!("server exited before client: {res:?}"),
                result = client_fut => {
                    assert!(
                        result.is_err(),
                        "bob must not be able to fork alice's session, got: {result:?}"
                    );
                }
            }
        })
        .await;
}

/// #6656 regression: a permission-gated tool call must not deadlock the `session/prompt`
/// round-trip. Before the fix, `handle_prompt` awaited `do_prompt` inline inside the ACP SDK's
/// serial request-dispatch loop; the same loop demultiplexes the client's reply to the
/// `session/request_permission` request sent by `AcpPermissionGate::check_permission`, so the
/// loop deadlocked on itself. Wrapped in a timeout so a regression fails this test fast instead
/// of hanging the suite.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::large_futures)]
async fn permission_gated_prompt_round_trip_does_not_deadlock() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let (decision_tx, mut decision_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
            let server_fut = serve_connection(
                permission_gated_spawner(decision_tx),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client
                .builder()
                .on_receive_request(
                    async |_req: acp::schema::v1::RequestPermissionRequest,
                           responder: acp::Responder<
                        acp::schema::v1::RequestPermissionResponse,
                    >,
                           _cx| {
                        responder.respond(acp::schema::v1::RequestPermissionResponse::new(
                            acp::schema::v1::RequestPermissionOutcome::Selected(
                                acp::schema::v1::SelectedPermissionOutcome::new("allow_once"),
                            ),
                        ))
                    },
                    acp::on_receive_request!(),
                )
                .connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                    cx.send_request(acp::schema::v1::InitializeRequest::new(
                        acp::schema::ProtocolVersion::LATEST,
                    ))
                    .block_task()
                    .await?;

                    let session_id = cx
                        .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                        .block_task()
                        .await?
                        .session_id;

                    let content = vec![acp::schema::v1::ContentBlock::Text(
                        acp::schema::v1::TextContent::new("run a gated tool"),
                    )];
                    let resp = cx
                        .send_request(acp::schema::v1::PromptRequest::new(session_id, content))
                        .block_task()
                        .await?;

                    assert_eq!(
                        resp.stop_reason,
                        acp::schema::v1::StopReason::EndTurn,
                        "expected EndTurn, got {:?}",
                        resp.stop_reason,
                    );
                    Ok(())
                });

            let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                tokio::select! {
                    res = server_fut => panic!("server exited before client: {res:?}"),
                    result = client_fut => result,
                }
            })
            .await;

            let result = outcome.expect(
                "permission-gated prompt round-trip timed out — likely a #6656 deadlock regression",
            );
            assert!(result.is_ok(), "prompt round-trip failed: {result:?}");

            let decision = decision_rx
                .recv()
                .await
                .expect("spawner must report a permission decision");
            assert!(
                decision,
                "IDE selected allow_once, expected the gate to allow"
            );
        })
        .await;
}

/// #6656 regression, denial path: same round-trip as
/// `permission_gated_prompt_round_trip_does_not_deadlock`, but the IDE rejects the tool call.
/// Confirms fail-closed behavior still completes correctly through the new `cx.spawn`-based
/// dispatch path instead of also deadlocking.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::large_futures)]
async fn permission_gated_prompt_denial_does_not_deadlock() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let workdir = temp_workdir();
            let (sw, sr, cw, cr) = duplex_pair();
            let (decision_tx, mut decision_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
            let server_fut = serve_connection(
                permission_gated_spawner(decision_tx),
                test_config("test-agent"),
                sw,
                sr,
                "acp-local".to_owned(),
            );
            let client_fut = acp::Client
                .builder()
                .on_receive_request(
                    async |_req: acp::schema::v1::RequestPermissionRequest,
                           responder: acp::Responder<
                        acp::schema::v1::RequestPermissionResponse,
                    >,
                           _cx| {
                        responder.respond(acp::schema::v1::RequestPermissionResponse::new(
                            acp::schema::v1::RequestPermissionOutcome::Selected(
                                acp::schema::v1::SelectedPermissionOutcome::new("reject_once"),
                            ),
                        ))
                    },
                    acp::on_receive_request!(),
                )
                .connect_with(acp::ByteStreams::new(cw, cr), async |cx| {
                    cx.send_request(acp::schema::v1::InitializeRequest::new(
                        acp::schema::ProtocolVersion::LATEST,
                    ))
                    .block_task()
                    .await?;

                    let session_id = cx
                        .send_request(acp::schema::v1::NewSessionRequest::new(workdir.path()))
                        .block_task()
                        .await?
                        .session_id;

                    let content = vec![acp::schema::v1::ContentBlock::Text(
                        acp::schema::v1::TextContent::new("run a gated tool"),
                    )];
                    let resp = cx
                        .send_request(acp::schema::v1::PromptRequest::new(session_id, content))
                        .block_task()
                        .await?;

                    assert_eq!(
                        resp.stop_reason,
                        acp::schema::v1::StopReason::EndTurn,
                        "expected EndTurn, got {:?}",
                        resp.stop_reason,
                    );
                    Ok(())
                });

            let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                tokio::select! {
                    res = server_fut => panic!("server exited before client: {res:?}"),
                    result = client_fut => result,
                }
            })
            .await;

            let result = outcome.expect(
                "permission-gated prompt round-trip timed out — likely a #6656 deadlock regression",
            );
            assert!(result.is_ok(), "prompt round-trip failed: {result:?}");

            let decision = decision_rx
                .recv()
                .await
                .expect("spawner must report a permission decision");
            assert!(
                !decision,
                "IDE selected reject_once, expected the gate to deny"
            );
        })
        .await;
}
