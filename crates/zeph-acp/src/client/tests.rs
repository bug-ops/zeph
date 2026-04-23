// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
// Run with: `cargo nextest run -p zeph-acp -E 'test(subagent)'`

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{AcpClientError, SubagentConfig};
use crate::client::transport;

// ─── Config / Spawn tests ──────────────────────────────────────────────────

#[test]
fn subagent_spawn_empty_command_invalid_config() {
    let cfg = SubagentConfig {
        command: String::new(),
        ..SubagentConfig::default()
    };
    let result = transport::spawn_child(&cfg);
    assert!(
        matches!(result, Err(AcpClientError::InvalidConfig(_))),
        "expected InvalidConfig for empty command"
    );
}

#[allow(unsafe_code)]
#[tokio::test]
async fn subagent_env_isolation() {
    use tokio::io::{AsyncReadExt, BufReader};

    // Inject a ZEPH_-prefixed sentinel into the parent environment, then confirm
    // that `spawn_child` (which calls `env_clear`) does NOT forward it to the child.
    // Without this injection the test would pass vacuously on a clean CI machine.
    let sentinel = "ZEPH_TEST_SECRET_ISOLATION";
    // SAFETY: test-only; the test binary runs single-threaded at this point.
    unsafe {
        std::env::set_var(sentinel, "must-not-appear");
    }

    let cfg = SubagentConfig {
        // `env` prints its environment to stdout; we read it directly because
        // spawn_child gives us piped stdout (not consumed by an ACP decoder).
        command: "env".to_owned(),
        ..SubagentConfig::default()
    };

    let spawned = transport::spawn_child(&cfg).expect("spawn failed");
    let mut child = spawned.child;
    drop(spawned.stdin);
    drop(spawned.stderr);

    let mut out = String::new();
    BufReader::new(spawned.stdout).read_to_string(&mut out).await.unwrap();
    drop(child.wait().await);

    // Clean up parent env regardless of assertion outcome.
    unsafe {
        std::env::remove_var(sentinel);
    }

    for line in out.lines() {
        assert!(
            !line.starts_with("ZEPH_"),
            "ZEPH_* env var must not be forwarded to sub-agent subprocess; got: {line}"
        );
    }
}

#[tokio::test]
async fn subagent_cwd_respected() {
    use tokio::io::{AsyncReadExt, BufReader};

    let tmp = tempfile::tempdir().unwrap();
    let cfg = SubagentConfig {
        command: "pwd".to_owned(),
        process_cwd: Some(tmp.path().to_owned()),
        ..SubagentConfig::default()
    };

    let spawned = transport::spawn_child(&cfg).expect("spawn failed");
    let mut child = spawned.child;
    drop(spawned.stdin);
    drop(spawned.stderr);

    let mut out = String::new();
    BufReader::new(spawned.stdout).read_to_string(&mut out).await.unwrap();
    drop(child.wait().await);
    let output = out;

    let printed = output.trim();
    // On macOS /tmp is a symlink to /private/tmp; resolve both sides.
    let actual = std::fs::canonicalize(printed).unwrap_or_else(|_| PathBuf::from(printed));
    let expected = std::fs::canonicalize(tmp.path()).unwrap_or_else(|_| tmp.path().to_owned());
    assert_eq!(actual, expected, "subprocess cwd must match SubagentConfig::process_cwd");
}

#[test]
fn subagent_zeph_env_key_rejected() {
    let mut env = BTreeMap::new();
    env.insert("ZEPH_API_KEY".to_owned(), "secret".to_owned());
    let cfg = SubagentConfig {
        command: "true".to_owned(),
        env,
        ..SubagentConfig::default()
    };
    let result = transport::spawn_child(&cfg);
    assert!(
        matches!(result, Err(AcpClientError::InvalidConfig(_))),
        "ZEPH_* keys in cfg.env must be rejected"
    );
}

// ─── Config helper tests ───────────────────────────────────────────────────

#[test]
fn subagent_effective_session_cwd_falls_back_to_process_cwd() {
    let p = PathBuf::from("/tmp/test");
    let cfg = SubagentConfig {
        command: "true".to_owned(),
        process_cwd: Some(p.clone()),
        session_cwd: None,
        ..SubagentConfig::default()
    };
    assert_eq!(cfg.effective_session_cwd(), p);
}

#[test]
fn subagent_effective_session_cwd_prefers_explicit_session_cwd() {
    let p = PathBuf::from("/tmp/session");
    let cfg = SubagentConfig {
        command: "true".to_owned(),
        process_cwd: Some(PathBuf::from("/tmp/proc")),
        session_cwd: Some(p.clone()),
        ..SubagentConfig::default()
    };
    assert_eq!(cfg.effective_session_cwd(), p);
}

// ─── Error variant tests ───────────────────────────────────────────────────

#[test]
fn subagent_error_display_driver_busy() {
    let err = AcpClientError::DriverBusy;
    assert!(err.to_string().contains("busy"));
}

#[test]
fn subagent_error_display_driver_died() {
    let err = AcpClientError::DriverDied;
    assert!(err.to_string().contains("unexpectedly"));
}

#[test]
fn subagent_error_display_timeout() {
    let err = AcpClientError::Timeout;
    assert!(err.to_string().contains("timed out"));
}

// ─── Spawn failure test ────────────────────────────────────────────────────

#[test]
fn subagent_spawn_nonexistent_binary_error() {
    let cfg = SubagentConfig {
        command: "__zeph_no_such_binary_xyz__".to_owned(),
        ..SubagentConfig::default()
    };
    let result = transport::spawn_child(&cfg);
    assert!(
        matches!(result, Err(AcpClientError::Spawn(_))),
        "expected Spawn error for nonexistent binary"
    );
}

// ─── RunOutcome structural test ───────────────────────────────────────────

#[test]
fn subagent_run_outcome_is_clone() {
    let outcome = super::RunOutcome {
        text: "hello".to_owned(),
        stop_reason: agent_client_protocol::schema::StopReason::EndTurn,
    };
    let cloned = outcome.clone();
    assert_eq!(cloned.text, "hello");
    assert_eq!(cloned.stop_reason, agent_client_protocol::schema::StopReason::EndTurn);
}

// ─── Cancel preemption test (mock-driver level) ───────────────────────────
//
// Verifies that `send_cancel()` can be issued while `read_update()` is blocked
// waiting for a driver response, and that the cancel command is delivered to
// the driver before the parked read resolves.
//
// The mock driver runs on the same tokio task; it receives ReadUpdate, parks the
// reply, then receives Cancel — confirming the channel is live — then finally
// sends StopReason::Cancelled as the read result.

#[tokio::test]
async fn subagent_channel_delivers_cancel_while_read_parked() {
    use futures::channel::mpsc;
    use futures::StreamExt;

    use crate::client::driver::SubagentCommand;
    use super::SubagentHandle;

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded::<SubagentCommand>();
    let join_handle = tokio::spawn(futures::future::pending::<()>());
    let session_id = agent_client_protocol::schema::SessionId::new("test-cancel");

    // Split cmd_tx so the handle and the cancel injector each have a sender.
    let cmd_tx_cancel = cmd_tx.clone();
    let mut handle = SubagentHandle::new_for_test(cmd_tx, join_handle, session_id);

    // Spawn read_update — enqueues ReadUpdate{reply} and blocks.
    let read_task = tokio::spawn(async move {
        handle.read_update().await
    });

    // Yield so the spawned task has a chance to enqueue ReadUpdate.
    tokio::task::yield_now().await;

    // Pick up the ReadUpdate command and park its reply.
    let read_reply = match cmd_rx.next().await.expect("expected ReadUpdate") {
        SubagentCommand::ReadUpdate { reply } => reply,
        _other => panic!("expected ReadUpdate, got a different command"),
    };

    // Inject a Cancel command into the channel while ReadUpdate reply is still parked.
    // This simulates `handle.send_cancel()` called concurrently by a second task.
    let (cancel_reply_tx, cancel_reply_rx) = tokio::sync::oneshot::channel();
    cmd_tx_cancel
        .unbounded_send(SubagentCommand::Cancel { reply: cancel_reply_tx })
        .expect("send Cancel");

    // The mock driver services the Cancel command that arrived while read was parked.
    let cancel_cmd = cmd_rx.next().await.expect("expected Cancel");
    match cancel_cmd {
        SubagentCommand::Cancel { reply } => {
            let _ = reply.send(Ok(()));
        }
        _other => panic!("expected Cancel, got a different command"),
    }
    // Verify send_cancel() resolved.
    cancel_reply_rx.await.expect("cancel reply dropped").expect("cancel failed");

    // Now resolve the parked ReadUpdate with StopReason::Cancelled.
    let _ = read_reply.send(Ok(agent_client_protocol::SessionMessage::StopReason(
        agent_client_protocol::schema::StopReason::Cancelled,
    )));

    let result = read_task.await.expect("read_task panicked");
    match result {
        Ok(agent_client_protocol::SessionMessage::StopReason(
            agent_client_protocol::schema::StopReason::Cancelled,
        )) => {}
        other => panic!("expected StopReason::Cancelled, got {other:?}"),
    }
}

// ─── Drain filter test (mock-driver level) ────────────────────────────────
//
// Verifies that `read_to_string()` returns whatever the driver places in the
// RunOutcome — i.e., the SubagentHandle channel plumbing delivers the result
// without corruption. The actual text-vs-thought filtering logic is exercised
// in the integration test `drain_until_stop_collects_text_chunks`.

#[tokio::test]
async fn subagent_drain_filter_ignores_non_text() {
    use futures::channel::mpsc;

    use crate::client::driver::SubagentCommand;
    use super::{RunOutcome, SubagentHandle};

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded::<SubagentCommand>();
    let join_handle = tokio::spawn(futures::future::pending::<()>());
    let session_id = agent_client_protocol::schema::SessionId::new("test-session-2");
    let mut handle = SubagentHandle::new_for_test(cmd_tx, join_handle, session_id);

    // Spawn read_to_string — it enqueues ReadToString and blocks.
    let read_task = tokio::spawn(async move {
        handle.read_to_string().await
    });

    tokio::task::yield_now().await;

    // Receive the ReadToString command and reply with a RunOutcome that contains
    // only text (thought/tool-call chunks are discarded by drain_until_stop inside
    // the real driver; here we verify the handle routing is transparent).
    let cmd = {
        use futures::StreamExt;
        cmd_rx.next().await.expect("expected ReadToString")
    };
    match cmd {
        SubagentCommand::ReadToString { reply } => {
            let _ = reply.send(Ok(RunOutcome {
                text: "text only".to_owned(),
                stop_reason: agent_client_protocol::schema::StopReason::EndTurn,
            }));
        }
        _other => panic!("expected ReadToString, got a different command"),
    }

    let outcome = read_task.await.expect("read_task panicked").expect("RunOutcome error");
    assert_eq!(outcome.text, "text only");
    assert_eq!(outcome.stop_reason, agent_client_protocol::schema::StopReason::EndTurn);
}
