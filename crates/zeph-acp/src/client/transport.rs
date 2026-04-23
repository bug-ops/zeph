// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Subprocess spawning with hardened stdio and minimal environment.
//!
//! `spawn_child` is the only entry point used by the driver. It calls
//! `env_clear()` before applying the whitelist so no `ZEPH_*` secrets leak
//! into the sub-agent process, and sets `kill_on_drop(true)` so the child is
//! always reaped when the `SpawnedChild` handle is dropped.

use std::process::Stdio;

use tokio::process::Command;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::client::{AcpClientError, SubagentConfig};

/// Default environment keys always forwarded to sub-agent subprocesses.
///
/// These are the minimum keys a typical Rust/cargo tool needs to operate.
/// `ZEPH_*` keys are explicitly excluded and rejected if present in `cfg.env`.
const DEFAULT_INHERIT: &[&str] = &["HOME", "PATH", "TMPDIR", "TERM", "LANG", "USER", "LOGNAME"];

/// Handles for a freshly-spawned sub-agent subprocess.
///
/// All stdio streams are extracted from `tokio::process::Child` before this
/// struct is constructed, so the child handle no longer owns them.
pub(crate) struct SpawnedChild {
    /// Process handle. `kill_on_drop(true)` is set so the child is always reaped.
    pub child: tokio::process::Child,
    /// Stdin pipe for JSON-RPC framing.
    pub stdin: tokio::process::ChildStdin,
    /// Stdout pipe for JSON-RPC framing.
    pub stdout: tokio::process::ChildStdout,
    /// Stderr pipe forwarded to the `stderr_drain` task.
    pub stderr: tokio::process::ChildStderr,
}

/// Create a `ByteStreams` transport from a `SpawnedChild`'s stdio pipes.
///
/// Consumes `stdin` and `stdout` from `SpawnedChild`; the caller retains
/// `child` and `stderr` for lifecycle management.
///
/// `ByteStreams::new(outgoing, incoming)`:
/// - outgoing (write) = stdin → `compat_write()` → `futures::AsyncWrite`
/// - incoming (read)  = stdout → `compat()` → `futures::AsyncRead`
pub(crate) fn make_byte_streams(
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
) -> agent_client_protocol::ByteStreams<
    tokio_util::compat::Compat<tokio::process::ChildStdin>,
    tokio_util::compat::Compat<tokio::process::ChildStdout>,
> {
    agent_client_protocol::ByteStreams::new(stdin.compat_write(), stdout.compat())
}

/// Spawn the sub-agent subprocess with a minimal environment and hardened stdio.
///
/// Steps applied in order:
/// 1. `shell_words::split(&cfg.command)` → (program, args).
/// 2. `Command::new(program).args(args)`.
/// 3. `.env_clear()` — without this `ZEPH_*` secrets leak from the parent.
/// 4. Apply `DEFAULT_INHERIT` keys present in the parent environment.
/// 5. Apply `cfg.inherit_env` keys present in the parent environment.
/// 6. Apply `cfg.env` (explicit key=value overrides; `ZEPH_*` rejected).
/// 7. `.current_dir(cfg.effective_process_cwd())`.
/// 8. `.stdin(piped()).stdout(piped()).stderr(piped())`.
/// 9. `.kill_on_drop(true)`.
/// 10. `.spawn()`.
///
/// # Errors
///
/// Returns [`AcpClientError::InvalidConfig`] when the command string is empty
/// or cannot be parsed, or [`AcpClientError::Spawn`] when the OS spawn fails.
pub(crate) fn spawn_child(cfg: &SubagentConfig) -> Result<SpawnedChild, AcpClientError> {
    let parts = shell_words::split(&cfg.command)
        .map_err(|e| AcpClientError::InvalidConfig(format!("shell_words parse error: {e}")))?;
    if parts.is_empty() {
        return Err(AcpClientError::InvalidConfig(
            "command string is empty".to_owned(),
        ));
    }
    let (program, args) = (&parts[0], &parts[1..]);

    let mut cmd = Command::new(program);
    cmd.args(args);

    // Security: start from a clean environment so no ZEPH_* secrets are forwarded.
    cmd.env_clear();

    // Apply default whitelist.
    for key in DEFAULT_INHERIT {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }

    // Apply caller-specified additional keys.
    for key in &cfg.inherit_env {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }

    // Apply explicit env overrides, rejecting ZEPH_* keys.
    for (k, v) in &cfg.env {
        if k.starts_with("ZEPH_") {
            return Err(AcpClientError::InvalidConfig(format!(
                "env key {k:?} starts with ZEPH_ and must not be forwarded to sub-agents"
            )));
        }
        cmd.env(k, v);
    }

    cmd.current_dir(cfg.effective_process_cwd());
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(AcpClientError::Spawn)?;

    let stdin = child.stdin.take().ok_or_else(|| {
        AcpClientError::InvalidConfig("failed to open subprocess stdin".to_owned())
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        AcpClientError::InvalidConfig("failed to open subprocess stdout".to_owned())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        AcpClientError::InvalidConfig("failed to open subprocess stderr".to_owned())
    })?;

    Ok(SpawnedChild {
        child,
        stdin,
        stdout,
        stderr,
    })
}

/// Spawn a background task that drains subprocess stderr line-by-line to tracing.
///
/// Lines are emitted at `tracing::debug!` with `target = "acp.client.stderr"` so
/// they can be filtered independently from the main agent log. The task exits when
/// the stderr pipe is closed (subprocess exit or `kill_on_drop`).
///
/// Returns a `JoinHandle` that can be aborted during driver shutdown.
pub(crate) fn spawn_stderr_drain(
    stderr: tokio::process::ChildStderr,
    session_hint: String,
) -> tokio::task::JoinHandle<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(
                target: "acp.client.stderr",
                session = %session_hint,
                "{line}"
            );
        }
    })
}
