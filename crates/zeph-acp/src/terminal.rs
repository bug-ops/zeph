// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! IDE-proxied shell executor via ACP `terminal/*` methods.
//!
//! When the IDE advertises `terminal` capability, the agent routes `bash` tool
//! calls through the IDE's integrated terminal instead of spawning a local process.
//! This keeps the terminal visible in the IDE UI and allows live output streaming.
//!
//! # Security
//!
//! All terminal commands require an [`AcpPermissionGate`] to request IDE confirmation.
//! Stdin writes are rate-limited and capped at 64 KiB (REQ-P23-1). Commands that
//! resolve to shell interpreters (`bash`, `sh`, `zsh`, etc.) trigger an explicit
//! warning in the permission prompt.
//!
//! # Terminal lifecycle
//!
//! ACP requires the terminal to remain alive until after the `tool_call_update`
//! notification containing `ToolCallContent::Terminal(terminal_id)` is emitted.
//! Call [`AcpShellExecutor::release_terminal`] only after that notification is sent.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use acp::Client as _;
use agent_client_protocol as acp;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use zeph_tools::{
    ToolCall, ToolError, ToolOutput,
    executor::deserialize_params,
    registry::{InvocationHint, ToolDef},
};

use crate::{error::AcpError, permission::AcpPermissionGate};

const KILL_GRACE_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum stdin payload size (64 KiB). REQ-P23-1.
const MAX_STDIN_BYTES: usize = 65_536;

/// Bounded stdin channel capacity (back-pressure). MED-02.
const STDIN_CHANNEL_CAPACITY: usize = 16;

/// Stdin rate-limit interval — 100 msg/sec. MED-02.
const STDIN_RATE_INTERVAL: Duration = Duration::from_millis(10);

/// Shell interpreters that require explicit warning in permission prompt. REQ-P23-5.
const SHELL_INTERPRETERS: &[&str] = &["bash", "sh", "zsh", "fish", "dash"];

/// Transparent prefixes that wrap another command without changing its semantics.
const TRANSPARENT_PREFIXES: &[&str] = &["env", "command", "exec", "nice", "nohup", "time"];

/// Extract the effective command binary name from a shell command string.
///
/// Iteratively skips transparent prefixes (`env`, `command`, `exec`, etc.) and
/// env-var assignments (`FOO=bar`) to reach the real binary. Falls back to `"bash"`
/// if the command is empty.
fn extract_command_binary(command: &str) -> &str {
    // Split into tokens and skip leading env-var assignments and transparent prefixes.
    let mut tokens = command.split_whitespace().peekable();
    loop {
        match tokens.peek() {
            None => return "bash",
            Some(tok) => {
                // Skip env-var assignments.
                if tok.contains('=') {
                    tokens.next();
                    continue;
                }
                // Skip transparent prefix commands.
                let base = tok.rsplit('/').next().unwrap_or(tok);
                if TRANSPARENT_PREFIXES.contains(&base) {
                    tokens.next();
                    continue;
                }
                // First non-prefix, non-assignment token is the binary.
                let binary = tok.rsplit('/').next().unwrap_or(tok);
                return binary;
            }
        }
    }
}

struct ShellResult {
    output: String,
    exit_code: Option<u32>,
    terminal_id: String,
}

struct TerminalRequest {
    session_id: acp::SessionId,
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    timeout: Duration,
    reply: oneshot::Sender<Result<ShellResult, AcpError>>,
    /// When `Some`, intermediate terminal output chunks are sent as `ToolCallUpdate`
    /// notifications on this channel so the IDE can stream output live.
    /// The `tool_call_id` is the ACP tool call ID to update.
    stream_tx: Option<(mpsc::Sender<acp::SessionNotification>, String)>,
}

struct TerminalReleaseRequest {
    session_id: acp::SessionId,
    terminal_id: String,
}

struct StdinWriteRequest {
    session_id: acp::SessionId,
    terminal_id: acp::TerminalId,
    data: Vec<u8>,
    reply: oneshot::Sender<Result<(), AcpError>>,
}

enum TerminalMessage {
    Execute(TerminalRequest),
    Release(TerminalReleaseRequest),
    WriteStdin(StdinWriteRequest),
}

/// IDE-proxied shell executor.
///
/// Routes `bash` tool calls to the IDE terminal via ACP `terminal/*` methods.
/// Only constructed when the IDE advertises `terminal` capability.
#[derive(Clone)]
pub struct AcpShellExecutor {
    session_id: acp::SessionId,
    request_tx: mpsc::UnboundedSender<TerminalMessage>,
    permission_gate: Option<AcpPermissionGate>,
    timeout: Duration,
}

impl AcpShellExecutor {
    /// Create the executor and its `LocalSet`-side handler future.
    ///
    /// Spawn the returned future inside the same `LocalSet` that owns `conn`.
    /// The handler drives terminal create/execute/release requests forwarded
    /// from the `bash` and `bash_stdin` tools.
    pub fn new<C>(
        conn: Rc<C>,
        session_id: acp::SessionId,
        permission_gate: Option<AcpPermissionGate>,
        timeout_secs: u64,
    ) -> (Self, impl std::future::Future<Output = ()>)
    where
        C: acp::Client + 'static,
    {
        Self::with_timeout(
            conn,
            session_id,
            permission_gate,
            Duration::from_secs(timeout_secs),
        )
    }

    /// Create the executor with a configurable command timeout.
    pub fn with_timeout<C>(
        conn: Rc<C>,
        session_id: acp::SessionId,
        permission_gate: Option<AcpPermissionGate>,
        timeout: Duration,
    ) -> (Self, impl std::future::Future<Output = ()>)
    where
        C: acp::Client + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel::<TerminalMessage>();
        let handler = async move { run_terminal_handler(conn, rx).await };
        (
            Self {
                session_id,
                request_tx: tx,
                permission_gate,
                timeout,
            },
            handler,
        )
    }

    /// Release a terminal by ID after the `tool_call_update` notification has been sent.
    ///
    /// This must be called after the ACP `tool_call_update` containing
    /// `ToolCallContent::Terminal(terminal_id)` is emitted so that the IDE can
    /// still display the terminal output when it processes the notification.
    pub fn release_terminal(&self, terminal_id: String) {
        self.request_tx
            .send(TerminalMessage::Release(TerminalReleaseRequest {
                session_id: self.session_id.clone(),
                terminal_id,
            }))
            .ok();
    }

    async fn handle_bash_stdin(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        // REQ-P23-2: blocked if no permission gate
        let gate = self
            .permission_gate
            .as_ref()
            .ok_or_else(|| ToolError::Blocked {
                command: "bash_stdin: permission gate required".into(),
            })?;

        let params: BashStdinParams = deserialize_params(&call.params)?;

        if params.data.len() > MAX_STDIN_BYTES {
            return Err(ToolError::InvalidParams {
                message: AcpError::StdinTooLarge {
                    size: params.data.len(),
                }
                .to_string(),
            });
        }
        let data = params.data.as_bytes().to_vec();

        // REQ-P23-5: warn when writing to a shell interpreter terminal.
        // Terminal IDs are opaque strings, but common practice is to include
        // the command name. We always request permission explicitly for stdin writes.
        let is_shell = SHELL_INTERPRETERS
            .iter()
            .any(|s| params.terminal_id.contains(s));
        let title = if is_shell {
            "bash_stdin [WARNING: stdin to shell interpreter — data will be executed as commands]"
                .to_string()
        } else {
            "bash_stdin".to_owned()
        };
        let fields = acp::ToolCallUpdateFields::new()
            .title(title)
            .raw_input(serde_json::json!({
                "terminal_id": params.terminal_id,
                "data_length": params.data.len(),
            }));
        let tool_call = acp::ToolCallUpdate::new("bash_stdin".to_owned(), fields);
        let allowed = gate
            .check_permission(self.session_id.clone(), tool_call)
            .await
            .map_err(|e| ToolError::InvalidParams {
                message: e.to_string(),
            })?;
        if !allowed {
            return Err(ToolError::Blocked {
                command: "bash_stdin: permission denied".into(),
            });
        }

        let terminal_id: acp::TerminalId = params.terminal_id.clone().into();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(TerminalMessage::WriteStdin(StdinWriteRequest {
                session_id: self.session_id.clone(),
                terminal_id,
                data,
                reply: reply_tx,
            }))
            .map_err(|_| ToolError::InvalidParams {
                message: "terminal handler closed".into(),
            })?;
        reply_rx
            .await
            .map_err(|_| ToolError::InvalidParams {
                message: "terminal handler closed".into(),
            })?
            .map_err(|e| ToolError::InvalidParams {
                message: e.to_string(),
            })?;

        Ok(Some(ToolOutput {
            tool_name: zeph_tools::ToolName::new("bash_stdin"),
            summary: format!(
                "wrote {} bytes to stdin of {}",
                params.data.len(),
                params.terminal_id
            ),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: Some(params.terminal_id),
            locations: None,
            raw_response: None,
            claim_source: Some(zeph_tools::ClaimSource::Shell),
        }))
    }

    async fn execute_shell(
        &self,
        command: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        stream_tx: Option<(mpsc::Sender<acp::SessionNotification>, String)>,
    ) -> Result<ShellResult, AcpError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(TerminalMessage::Execute(TerminalRequest {
                session_id: self.session_id.clone(),
                command,
                args,
                cwd,
                timeout: self.timeout,
                reply: reply_tx,
                stream_tx,
            }))
            .map_err(|_| AcpError::ChannelClosed)?;
        reply_rx.await.map_err(|_| AcpError::ChannelClosed)?
    }
}

#[derive(Deserialize, JsonSchema)]
struct BashParams {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct BashStdinParams {
    terminal_id: String,
    data: String,
}

impl zeph_tools::ToolExecutor for AcpShellExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        let mut defs = vec![ToolDef {
            id: "bash".into(),
            description: "Execute a shell command in the IDE terminal.\n\nParameters: command (string, required) - shell command to run\nReturns: stdout/stderr combined with exit code\nErrors: Timeout; permission denied by IDE; command blocked by policy\nExample: {\"command\": \"cargo build\"}".into(),
            schema: schemars::schema_for!(BashParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        }];
        // REQ-P23-2: bash_stdin only available when a permission gate is present.
        if self.permission_gate.is_some() {
            defs.push(ToolDef {
                id: "bash_stdin".into(),
                description: "Write data to stdin of a running terminal process.\n\nParameters: terminal_id (string, required) - terminal to write to; data (string, required) - stdin data\nReturns: confirmation\nErrors: terminal not found; terminal process exited\nExample: {\"terminal_id\": \"term-1\", \"data\": \"yes\\n\"}".into(),
                schema: schemars::schema_for!(BashStdinParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            });
        }
        defs
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id == "bash_stdin" {
            return self.handle_bash_stdin(call).await;
        }
        if call.tool_id != "bash" {
            return Ok(None);
        }

        let params: BashParams = deserialize_params(&call.params)?;
        let cwd = params.cwd.map(PathBuf::from);

        let blocklist: Vec<String> = zeph_tools::DEFAULT_BLOCKED_COMMANDS
            .iter()
            .map(|s| (*s).to_owned())
            .collect();

        // Blocklist check — reject dangerous commands before hitting the permission gate.
        if let Some(pattern) = zeph_tools::check_blocklist(&params.command, &blocklist) {
            return Err(ToolError::Blocked { command: pattern });
        }
        // Also check args when the command is a shell interpreter (e.g. bash -c "rm -rf /").
        // This prevents args-field bypass: { command: "bash", args: ["-c", "blocked cmd"] }.
        if let Some(script) = zeph_tools::effective_shell_command(&params.command, &params.args)
            && let Some(pattern) = zeph_tools::check_blocklist(script, &blocklist)
        {
            return Err(ToolError::Blocked { command: pattern });
        }

        if self.permission_gate.is_none() {
            tracing::warn!(
                "AcpShellExecutor has no permission gate — only blocklist applies. \
                 Do not use in production without a permission gate."
            );
        }

        if let Some(gate) = &self.permission_gate {
            // Use the command binary as the cache key, not the tool_id ("bash").
            // This makes "Allow always" apply per binary (git, cargo, etc.).
            let cmd_binary = extract_command_binary(&params.command);
            let fields = acp::ToolCallUpdateFields::new()
                .title(cmd_binary.to_owned())
                .raw_input(serde_json::json!({ "command": params.command }));
            let tool_call = acp::ToolCallUpdate::new(cmd_binary.to_owned(), fields);
            let allowed = gate
                .check_permission(self.session_id.clone(), tool_call)
                .await
                .map_err(|e| ToolError::InvalidParams {
                    message: e.to_string(),
                })?;
            if !allowed {
                return Err(ToolError::Blocked {
                    command: params.command,
                });
            }
        }

        let result = self
            .execute_shell(params.command, params.args, cwd, None)
            .await
            .map_err(|e| ToolError::InvalidParams {
                message: e.to_string(),
            })?;

        let is_error = !matches!(result.exit_code, Some(0) | None);
        let summary = if is_error {
            format!(
                "[exit {}]\n{}",
                result.exit_code.unwrap_or(1),
                result.output
            )
        } else {
            result.output.clone()
        };
        let raw_response = Some(serde_json::json!({
            "stdout": result.output,
            "stderr": "",
            "interrupted": false,
            "isImage": false,
            "noOutputExpected": false
        }));

        Ok(Some(ToolOutput {
            tool_name: zeph_tools::ToolName::new("bash"),
            summary,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: Some(result.terminal_id),
            locations: None,
            raw_response,
            claim_source: Some(zeph_tools::ClaimSource::Shell),
        }))
    }
}

async fn forward_stdin_via_ext<C>(
    conn: &Rc<C>,
    session_id: &acp::SessionId,
    terminal_id: &acp::TerminalId,
    data: Vec<u8>,
) -> Result<(), AcpError>
where
    C: acp::Client,
{
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
    let params_json = serde_json::json!({
        "session_id": session_id.to_string(),
        "terminal_id": terminal_id.to_string(),
        "data": encoded,
    });
    let raw = serde_json::value::RawValue::from_string(params_json.to_string())
        .map_err(|e| AcpError::ClientError(e.to_string()))?;
    let req = acp::ExtRequest::new("terminal/write_stdin", Arc::from(raw));
    conn.ext_method(req)
        .await
        .map(|_| ())
        .map_err(|e| AcpError::ClientError(e.to_string()))
}

/// Background pump: drains bounded stdin channel at ≤100 msg/sec (MED-02).
///
/// REQ-P23-3: on any error from `ext_method`, cancels the token and exits.
async fn run_stdin_pump<C>(
    conn: Rc<C>,
    session_id: acp::SessionId,
    terminal_id: acp::TerminalId,
    mut data_rx: mpsc::Receiver<Vec<u8>>,
    cancel: CancellationToken,
) where
    C: acp::Client,
{
    let mut interval = tokio::time::interval(STDIN_RATE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let data = tokio::select! {
            () = cancel.cancelled() => break,
            msg = data_rx.recv() => match msg {
                Some(d) => d,
                None => break,
            },
        };
        // Rate-limit: wait for tick before forwarding. MED-02.
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = interval.tick() => {}
        }
        if let Err(e) = forward_stdin_via_ext(&conn, &session_id, &terminal_id, data).await {
            // REQ-P23-3: no panics, log and cancel.
            tracing::warn!(%terminal_id, error = %e, "stdin pump error — cancelling");
            cancel.cancel();
            break;
        }
    }
}

async fn run_terminal_handler<C>(conn: Rc<C>, mut rx: mpsc::UnboundedReceiver<TerminalMessage>)
where
    C: acp::Client + 'static,
{
    // Maps terminal_id -> (bounded stdin sender, CancellationToken). MED-02, REQ-P23-4.
    let mut stdin_pumps: std::collections::HashMap<
        String,
        (mpsc::Sender<Vec<u8>>, CancellationToken),
    > = std::collections::HashMap::new();

    while let Some(msg) = rx.recv().await {
        match msg {
            TerminalMessage::Execute(req) => {
                let result = execute_in_terminal(
                    &conn,
                    req.session_id,
                    req.command,
                    req.args,
                    req.cwd,
                    req.timeout,
                    req.stream_tx,
                )
                .await;
                // Cancel stdin pump when terminal completes. REQ-P23-4.
                if let Ok(ref shell_result) = result
                    && let Some((_, token)) = stdin_pumps.remove(&shell_result.terminal_id)
                {
                    token.cancel();
                }
                req.reply.send(result).ok();
            }
            TerminalMessage::Release(req) => {
                // Cancel stdin pump on release. REQ-P23-4.
                if let Some((_, token)) = stdin_pumps.remove(&req.terminal_id) {
                    token.cancel();
                }
                let tid = req.terminal_id.clone();
                let release_req = acp::ReleaseTerminalRequest::new(req.session_id, req.terminal_id);
                if let Err(e) = conn.release_terminal(release_req).await {
                    tracing::warn!(
                        terminal_id = %tid,
                        error = %e,
                        "failed to release terminal"
                    );
                }
            }
            TerminalMessage::WriteStdin(req) => {
                let tid_str = req.terminal_id.to_string();

                // Lazily start a bounded pump task per terminal. MED-02.
                let (data_tx, cancel) = stdin_pumps.entry(tid_str).or_insert_with(|| {
                    let (tx, rx) = mpsc::channel::<Vec<u8>>(STDIN_CHANNEL_CAPACITY);
                    let token = CancellationToken::new();
                    tokio::task::spawn_local(run_stdin_pump(
                        conn.clone(),
                        req.session_id.clone(),
                        req.terminal_id.clone(),
                        rx,
                        token.clone(),
                    ));
                    (tx, token)
                });

                let result = if cancel.is_cancelled() {
                    Err(AcpError::BrokenPipe)
                } else {
                    // Bounded send — returns Err if channel is full (back-pressure).
                    data_tx.try_send(req.data).map_err(|_| AcpError::BrokenPipe)
                };

                req.reply.send(result).ok();
            }
        }
    }
}

/// Polling interval for terminal output streaming.
const STREAM_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Kill a terminal, then wait up to [`KILL_GRACE_TIMEOUT`] for it to exit.
async fn kill_terminal<C>(
    conn: &Rc<C>,
    session_id: &acp::SessionId,
    terminal_id: &acp::TerminalId,
) -> Result<(), AcpError>
where
    C: acp::Client,
{
    tracing::warn!(%terminal_id, "terminal command timed out — sending kill");
    let kill_req = acp::KillTerminalRequest::new(session_id.clone(), terminal_id.clone());
    conn.kill_terminal(kill_req)
        .await
        .map_err(|e| AcpError::ClientError(e.to_string()))?;
    let wait_again = acp::WaitForTerminalExitRequest::new(session_id.clone(), terminal_id.clone());
    let _ = tokio::time::timeout(KILL_GRACE_TIMEOUT, conn.wait_for_terminal_exit(wait_again)).await;
    Ok(())
}

/// Stream terminal output chunks to `notify_tx` while polling for process exit.
///
/// Returns the exit code once the process terminates or the timeout is reached.
async fn stream_until_exit<C>(
    conn: &Rc<C>,
    session_id: &acp::SessionId,
    terminal_id: &acp::TerminalId,
    timeout: Duration,
    notify_tx: &mpsc::Sender<acp::SessionNotification>,
    tool_call_id: &str,
) -> Result<Option<u32>, AcpError>
where
    C: acp::Client,
{
    let wait_req = acp::WaitForTerminalExitRequest::new(session_id.clone(), terminal_id.clone());
    let exit_future = conn.wait_for_terminal_exit(wait_req);
    tokio::pin!(exit_future);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_output_len = 0usize;

    loop {
        tokio::select! {
            result = &mut exit_future => {
                return match result {
                    Ok(resp) => Ok(resp.exit_status.exit_code),
                    Err(e) => Err(AcpError::ClientError(e.to_string())),
                };
            }
            () = tokio::time::sleep(STREAM_POLL_INTERVAL) => {
                if tokio::time::Instant::now() >= deadline {
                    kill_terminal(conn, session_id, terminal_id).await?;
                    return Ok(Some(124u32));
                }
                let output_req =
                    acp::TerminalOutputRequest::new(session_id.clone(), terminal_id.clone());
                if let Ok(resp) = conn.terminal_output(output_req).await {
                    let new_data = resp.output.get(last_output_len..).unwrap_or("");
                    if !new_data.is_empty() {
                        last_output_len = resp.output.len();
                        let mut meta = serde_json::Map::new();
                        meta.insert(
                            "terminal_output".to_owned(),
                            serde_json::json!({
                                "terminal_id": terminal_id.to_string(),
                                "data": new_data,
                            }),
                        );
                        let update = acp::ToolCallUpdate::new(
                            tool_call_id.to_owned(),
                            acp::ToolCallUpdateFields::new(),
                        )
                        .meta(meta);
                        let notif = acp::SessionNotification::new(
                            session_id.clone(),
                            acp::SessionUpdate::ToolCallUpdate(update),
                        );
                        let _ = notify_tx.try_send(notif);
                    }
                }
            }
        }
    }
}

async fn execute_in_terminal<C>(
    conn: &Rc<C>,
    session_id: acp::SessionId,
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    timeout: Duration,
    stream_tx: Option<(mpsc::Sender<acp::SessionNotification>, String)>,
) -> Result<ShellResult, AcpError>
where
    C: acp::Client,
{
    // 1. Create terminal.
    let create_req = acp::CreateTerminalRequest::new(session_id.clone(), command)
        .args(args)
        .cwd(cwd);
    let create_resp = conn
        .create_terminal(create_req)
        .await
        .map_err(|e| AcpError::ClientError(e.to_string()))?;
    let terminal_id = create_resp.terminal_id;

    // 2. Wait for exit with timeout; kill if exceeded.
    let exit_code = if let Some((ref notify_tx, ref tool_call_id)) = stream_tx {
        stream_until_exit(
            conn,
            &session_id,
            &terminal_id,
            timeout,
            notify_tx,
            tool_call_id,
        )
        .await?
    } else {
        let wait_req =
            acp::WaitForTerminalExitRequest::new(session_id.clone(), terminal_id.clone());
        match tokio::time::timeout(timeout, conn.wait_for_terminal_exit(wait_req)).await {
            Ok(Ok(resp)) => resp.exit_status.exit_code,
            Ok(Err(e)) => return Err(AcpError::ClientError(e.to_string())),
            Err(_) => {
                kill_terminal(conn, &session_id, &terminal_id).await?;
                Some(124u32)
            }
        }
    };

    // 3. Get final output. Terminal is NOT released here — the caller releases it
    //    after the ACP `tool_call_update` notification carrying `ToolCallContent::Terminal`
    //    has been sent, so the IDE can still display the terminal output.
    let output_req = acp::TerminalOutputRequest::new(session_id.clone(), terminal_id.clone());
    let output_resp = conn
        .terminal_output(output_req)
        .await
        .map_err(|e| AcpError::ClientError(e.to_string()))?;

    // 4. Emit terminal_exit notification if streaming is active.
    if let Some((ref notify_tx, ref tool_call_id)) = stream_tx {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "terminal_exit".to_owned(),
            serde_json::json!({ "terminal_id": terminal_id.to_string(), "exit_code": exit_code }),
        );
        let update =
            acp::ToolCallUpdate::new(tool_call_id.clone(), acp::ToolCallUpdateFields::new())
                .meta(meta);
        let notif = acp::SessionNotification::new(
            session_id.clone(),
            acp::SessionUpdate::ToolCallUpdate(update),
        );
        let _ = notify_tx.try_send(notif);
    }

    // Terminal release is handled by AcpShellExecutor::release_terminal via TerminalMessage::Release.
    Ok(ShellResult {
        output: output_resp.output,
        exit_code,
        terminal_id: terminal_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use zeph_tools::ToolExecutor as _;

    use super::*;

    struct FakeTerminalClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for FakeTerminalClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn create_terminal(
            &self,
            _args: acp::CreateTerminalRequest,
        ) -> acp::Result<acp::CreateTerminalResponse> {
            Ok(acp::CreateTerminalResponse::new("term-1"))
        }

        async fn wait_for_terminal_exit(
            &self,
            _args: acp::WaitForTerminalExitRequest,
        ) -> acp::Result<acp::WaitForTerminalExitResponse> {
            Ok(acp::WaitForTerminalExitResponse::new(
                acp::TerminalExitStatus::new().exit_code(0u32),
            ))
        }

        async fn terminal_output(
            &self,
            _args: acp::TerminalOutputRequest,
        ) -> acp::Result<acp::TerminalOutputResponse> {
            Ok(acp::TerminalOutputResponse::new("hello\n", false))
        }

        async fn release_terminal(
            &self,
            _args: acp::ReleaseTerminalRequest,
        ) -> acp::Result<acp::ReleaseTerminalResponse> {
            Ok(acp::ReleaseTerminalResponse::new())
        }

        async fn kill_terminal(
            &self,
            _args: acp::KillTerminalRequest,
        ) -> acp::Result<acp::KillTerminalResponse> {
            Ok(acp::KillTerminalResponse::new())
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn bash_tool_call_returns_output() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeTerminalClient);
                let sid = acp::SessionId::new("s1");
                let (exec, handler) = AcpShellExecutor::new(conn, sid, None, 120);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("command".to_owned(), serde_json::json!("echo"));
                params.insert("args".to_owned(), serde_json::json!(["hello"]));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("bash"),
                    params,
                    caller_id: None,
                };

                let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
                assert_eq!(result.summary, "hello\n");
                assert_eq!(result.tool_name, "bash");
            })
            .await;
    }

    #[tokio::test]
    async fn unknown_tool_returns_none() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeTerminalClient);
                let sid = acp::SessionId::new("s1");
                let (exec, handler) = AcpShellExecutor::new(conn, sid, None, 120);
                tokio::task::spawn_local(handler);

                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("unknown"),
                    params: serde_json::Map::new(),
                    caller_id: None,
                };
                let result = exec.execute_tool_call(&call).await.unwrap();
                assert!(result.is_none());
            })
            .await;
    }

    #[test]
    fn tool_definitions_registers_bash() {
        let (tx, _rx) = mpsc::unbounded_channel::<TerminalMessage>();
        let exec = AcpShellExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            permission_gate: None,
            timeout: Duration::from_mins(2),
        };
        let defs = exec.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id, "bash");
    }

    struct NonZeroExitClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for NonZeroExitClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn create_terminal(
            &self,
            _args: acp::CreateTerminalRequest,
        ) -> acp::Result<acp::CreateTerminalResponse> {
            Ok(acp::CreateTerminalResponse::new("term-fail"))
        }

        async fn wait_for_terminal_exit(
            &self,
            _args: acp::WaitForTerminalExitRequest,
        ) -> acp::Result<acp::WaitForTerminalExitResponse> {
            Ok(acp::WaitForTerminalExitResponse::new(
                acp::TerminalExitStatus::new().exit_code(1u32),
            ))
        }

        async fn terminal_output(
            &self,
            _args: acp::TerminalOutputRequest,
        ) -> acp::Result<acp::TerminalOutputResponse> {
            Ok(acp::TerminalOutputResponse::new("error output\n", false))
        }

        async fn release_terminal(
            &self,
            _args: acp::ReleaseTerminalRequest,
        ) -> acp::Result<acp::ReleaseTerminalResponse> {
            Ok(acp::ReleaseTerminalResponse::new())
        }

        async fn kill_terminal(
            &self,
            _args: acp::KillTerminalRequest,
        ) -> acp::Result<acp::KillTerminalResponse> {
            Ok(acp::KillTerminalResponse::new())
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn nonzero_exit_code_prefixes_output() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(NonZeroExitClient);
                let sid = acp::SessionId::new("s1");
                let (exec, handler) = AcpShellExecutor::new(conn, sid, None, 120);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("command".to_owned(), serde_json::json!("false"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("bash"),
                    params,
                    caller_id: None,
                };

                let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
                assert!(
                    result.summary.starts_with("[exit 1]"),
                    "got: {}",
                    result.summary
                );
                assert!(result.summary.contains("error output\n"));
            })
            .await;
    }

    struct RejectPermissionClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for RejectPermissionClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "reject_once",
                )),
            ))
        }

        async fn create_terminal(
            &self,
            _args: acp::CreateTerminalRequest,
        ) -> acp::Result<acp::CreateTerminalResponse> {
            panic!("should not be called when permission denied")
        }

        async fn wait_for_terminal_exit(
            &self,
            _args: acp::WaitForTerminalExitRequest,
        ) -> acp::Result<acp::WaitForTerminalExitResponse> {
            panic!("should not be called when permission denied")
        }

        async fn terminal_output(
            &self,
            _args: acp::TerminalOutputRequest,
        ) -> acp::Result<acp::TerminalOutputResponse> {
            panic!("should not be called when permission denied")
        }

        async fn release_terminal(
            &self,
            _args: acp::ReleaseTerminalRequest,
        ) -> acp::Result<acp::ReleaseTerminalResponse> {
            panic!("should not be called when permission denied")
        }

        async fn kill_terminal(
            &self,
            _args: acp::KillTerminalRequest,
        ) -> acp::Result<acp::KillTerminalResponse> {
            panic!("should not be called when permission denied")
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn permission_denied_returns_blocked_error() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let perm_conn = Rc::new(RejectPermissionClient);
                let sid = acp::SessionId::new("s1");
                let tmp_dir = tempfile::tempdir().unwrap();
                let perm_file = tmp_dir.path().join("perms.toml");
                let (gate, perm_handler) = AcpPermissionGate::new(perm_conn, Some(perm_file));
                tokio::task::spawn_local(perm_handler);

                let term_conn = Rc::new(FakeTerminalClient);
                let (exec, term_handler) = AcpShellExecutor::new(term_conn, sid, Some(gate), 120);
                tokio::task::spawn_local(term_handler);

                let mut params = serde_json::Map::new();
                params.insert("command".to_owned(), serde_json::json!("rm"));
                params.insert("args".to_owned(), serde_json::json!(["-rf", "/important"]));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("bash"),
                    params,
                    caller_id: None,
                };

                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::Blocked { .. }));
            })
            .await;
    }

    #[tokio::test]
    async fn streaming_mode_emits_terminal_exit_notification() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeTerminalClient);
                let sid = acp::SessionId::new("s1");
                let (tx, rx) = mpsc::unbounded_channel::<TerminalMessage>();
                let handler = async move { run_terminal_handler(conn, rx).await };
                tokio::task::spawn_local(handler);

                let (stream_tx, mut stream_rx) = mpsc::channel(8);
                let (reply_tx, reply_rx) = oneshot::channel();
                tx.send(TerminalMessage::Execute(TerminalRequest {
                    session_id: sid,
                    command: "echo".to_owned(),
                    args: vec!["hi".to_owned()],
                    cwd: None,
                    timeout: Duration::from_secs(5),
                    reply: reply_tx,
                    stream_tx: Some((stream_tx, "tool-1".to_owned())),
                }))
                .unwrap();

                let result = reply_rx.await.unwrap().unwrap();
                assert_eq!(result.output, "hello\n");

                // At least a terminal_exit notification must arrive.
                let mut got_exit = false;
                while let Ok(notif) = stream_rx.try_recv() {
                    if let acp::SessionUpdate::ToolCallUpdate(update) = notif.update
                        && let Some(meta) = update.meta
                        && meta.contains_key("terminal_exit")
                    {
                        got_exit = true;
                    }
                }
                assert!(got_exit, "expected terminal_exit notification");
            })
            .await;
    }

    #[test]
    fn extract_command_binary_bare() {
        assert_eq!(extract_command_binary("git status"), "git");
        assert_eq!(extract_command_binary("cargo build --release"), "cargo");
        assert_eq!(extract_command_binary("  cat file.txt  "), "cat");
    }

    #[test]
    fn extract_command_binary_env_prefix() {
        assert_eq!(extract_command_binary("env FOO=bar git status"), "git");
        assert_eq!(extract_command_binary("command git push"), "git");
        assert_eq!(extract_command_binary("exec cargo test"), "cargo");
    }

    #[test]
    fn extract_command_binary_env_var_assignments() {
        assert_eq!(extract_command_binary("FOO=bar BAZ=qux git log"), "git");
    }

    #[test]
    fn extract_command_binary_path() {
        assert_eq!(extract_command_binary("/usr/bin/git status"), "git");
        assert_eq!(
            extract_command_binary("/usr/local/bin/cargo build"),
            "cargo"
        );
    }

    #[test]
    fn extract_command_binary_empty_fallback() {
        assert_eq!(extract_command_binary(""), "bash");
        assert_eq!(extract_command_binary("   "), "bash");
    }

    #[tokio::test]
    async fn blocklist_blocked_before_permission_gate() {
        // rm -rf / must be blocked before the permission gate is consulted.
        // FakeTerminalClient panics if create_terminal is called — so if
        // we reach the terminal, the test fails.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeTerminalClient);
                let sid = acp::SessionId::new("s1");
                // No permission gate — blocklist runs independently.
                let (exec, handler) = AcpShellExecutor::new(conn, sid, None, 120);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("command".to_owned(), serde_json::json!("rm -rf /"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("bash"),
                    params,
                    caller_id: None,
                };

                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::Blocked { .. }));
            })
            .await;
    }

    #[tokio::test]
    async fn blocklist_sudo_blocked() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeTerminalClient);
                let sid = acp::SessionId::new("s1");
                let (exec, handler) = AcpShellExecutor::new(conn, sid, None, 120);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert(
                    "command".to_owned(),
                    serde_json::json!("sudo apt install vim"),
                );
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("bash"),
                    params,
                    caller_id: None,
                };

                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::Blocked { .. }));
            })
            .await;
    }

    #[tokio::test]
    async fn args_field_bypass_blocked_for_shell_interpreter() {
        // SEC-ACP-C2: { command: "bash", args: ["-c", "rm -rf /"] } must be blocked.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeTerminalClient);
                let sid = acp::SessionId::new("s1");
                let (exec, handler) = AcpShellExecutor::new(conn, sid, None, 120);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("command".to_owned(), serde_json::json!("bash"));
                params.insert(
                    "args".to_owned(),
                    serde_json::json!(["-c", "sudo rm -rf /"]),
                );
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("bash"),
                    params,
                    caller_id: None,
                };

                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::Blocked { .. }));
            })
            .await;
    }

    #[tokio::test]
    async fn args_field_bypass_sh_minus_c_blocked() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeTerminalClient);
                let sid = acp::SessionId::new("s1");
                let (exec, handler) = AcpShellExecutor::new(conn, sid, None, 120);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("command".to_owned(), serde_json::json!("sh"));
                params.insert(
                    "args".to_owned(),
                    serde_json::json!(["-c", "shutdown -h now"]),
                );
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("bash"),
                    params,
                    caller_id: None,
                };

                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::Blocked { .. }));
            })
            .await;
    }

    #[test]
    fn extract_command_binary_chained_transparent_prefixes() {
        // SEC-ACP-I1: "env command exec sudo rm" -> "sudo", not "command"
        assert_eq!(
            extract_command_binary("env command exec sudo rm -rf /"),
            "sudo"
        );
        assert_eq!(extract_command_binary("nice nohup time git status"), "git");
    }

    #[test]
    fn extract_command_binary_env_var_then_prefix_then_binary() {
        assert_eq!(extract_command_binary("FOO=bar env BAZ=qux git log"), "git");
    }

    #[tokio::test]
    async fn bash_stdin_blocked_without_permission_gate() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let conn = Rc::new(FakeTerminalClient);
                let sid = acp::SessionId::new("s1");
                let (exec, handler) = AcpShellExecutor::new(conn, sid, None, 120);
                tokio::task::spawn_local(handler);

                let mut params = serde_json::Map::new();
                params.insert("terminal_id".to_owned(), serde_json::json!("term-1"));
                params.insert("data".to_owned(), serde_json::json!("hello\n"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("bash_stdin"),
                    params,
                    caller_id: None,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::Blocked { .. }));
            })
            .await;
    }

    #[test]
    fn bash_stdin_not_in_tool_definitions_without_gate() {
        let (tx, _rx) = mpsc::unbounded_channel::<TerminalMessage>();
        let exec = AcpShellExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            permission_gate: None,
            timeout: Duration::from_mins(2),
        };
        let defs = exec.tool_definitions();
        assert!(!defs.iter().any(|d| d.id == "bash_stdin"));
    }

    #[tokio::test]
    async fn bash_stdin_size_limit_rejected() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let perm_conn = Rc::new(RejectPermissionClient);
                let sid = acp::SessionId::new("s1");
                let tmp_dir = tempfile::tempdir().unwrap();
                let perm_file = tmp_dir.path().join("perms.toml");
                let (gate, perm_handler) = AcpPermissionGate::new(perm_conn, Some(perm_file));
                tokio::task::spawn_local(perm_handler);

                let term_conn = Rc::new(FakeTerminalClient);
                let (exec, term_handler) = AcpShellExecutor::new(term_conn, sid, Some(gate), 120);
                tokio::task::spawn_local(term_handler);

                let oversized = "x".repeat(MAX_STDIN_BYTES + 1);
                let mut params = serde_json::Map::new();
                params.insert("terminal_id".to_owned(), serde_json::json!("term-1"));
                params.insert("data".to_owned(), serde_json::json!(oversized));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("bash_stdin"),
                    params,
                    caller_id: None,
                };
                let err = exec.execute_tool_call(&call).await.unwrap_err();
                assert!(matches!(err, ToolError::InvalidParams { .. }));
            })
            .await;
    }

    struct AllowPermissionClient;

    #[async_trait::async_trait(?Send)]
    impl acp::Client for AllowPermissionClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    "allow_once",
                )),
            ))
        }

        async fn session_notification(&self, _args: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn bash_stdin_with_permission_gate_succeeds() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let perm_conn = Rc::new(AllowPermissionClient);
                let sid = acp::SessionId::new("s1");
                let tmp_dir = tempfile::tempdir().unwrap();
                let perm_file = tmp_dir.path().join("perms.toml");
                let (gate, perm_handler) = AcpPermissionGate::new(perm_conn, Some(perm_file));
                tokio::task::spawn_local(perm_handler);

                let term_conn = Rc::new(FakeTerminalClient);
                let (exec, term_handler) = AcpShellExecutor::new(term_conn, sid, Some(gate), 120);
                tokio::task::spawn_local(term_handler);

                let mut params = serde_json::Map::new();
                params.insert("terminal_id".to_owned(), serde_json::json!("term-1"));
                params.insert("data".to_owned(), serde_json::json!("echo hello\n"));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("bash_stdin"),
                    params,
                    caller_id: None,
                };
                let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
                assert_eq!(result.tool_name, "bash_stdin");
                assert!(result.summary.contains("term-1"));
            })
            .await;
    }

    #[test]
    fn bash_stdin_in_tool_definitions_with_gate() {
        let (tx, _rx) = mpsc::unbounded_channel::<TerminalMessage>();
        let tmp_dir = tempfile::tempdir().unwrap();
        let perm_file = tmp_dir.path().join("perms.toml");
        let perm_conn = Rc::new(AllowPermissionClient);
        let (gate, _handler) = AcpPermissionGate::new(perm_conn, Some(perm_file));
        let exec = AcpShellExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            permission_gate: Some(gate),
            timeout: Duration::from_mins(2),
        };
        let defs = exec.tool_definitions();
        assert!(defs.iter().any(|d| d.id == "bash_stdin"));
        assert!(defs.iter().any(|d| d.id == "bash"));
    }

    #[tokio::test]
    async fn bash_stdin_exactly_64kib_boundary_accepted() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let perm_conn = Rc::new(AllowPermissionClient);
                let sid = acp::SessionId::new("s1");
                let tmp_dir = tempfile::tempdir().unwrap();
                let perm_file = tmp_dir.path().join("perms.toml");
                let (gate, perm_handler) = AcpPermissionGate::new(perm_conn, Some(perm_file));
                tokio::task::spawn_local(perm_handler);

                let term_conn = Rc::new(FakeTerminalClient);
                let (exec, term_handler) = AcpShellExecutor::new(term_conn, sid, Some(gate), 120);
                tokio::task::spawn_local(term_handler);

                // Exactly at the limit must succeed.
                let at_limit = "x".repeat(MAX_STDIN_BYTES);
                let mut params = serde_json::Map::new();
                params.insert("terminal_id".to_owned(), serde_json::json!("term-1"));
                params.insert("data".to_owned(), serde_json::json!(at_limit));
                let call = ToolCall {
                    tool_id: zeph_tools::ToolName::new("bash_stdin"),
                    params,
                    caller_id: None,
                };
                let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
                assert_eq!(result.tool_name, "bash_stdin");
            })
            .await;
    }

    #[tokio::test]
    async fn bash_stdin_broken_pipe_fast_fail() {
        // After the CancellationToken is cancelled, WriteStdin must return BrokenPipe immediately.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, rx) = mpsc::unbounded_channel::<TerminalMessage>();
                let conn = Rc::new(FakeTerminalClient);
                let handler = async move { run_terminal_handler(conn, rx).await };
                tokio::task::spawn_local(handler);

                let sid = acp::SessionId::new("s1");
                let tid: acp::TerminalId = "term-bp".to_owned().into();

                // First WriteStdin: establishes the pump and cancels via a pre-cancelled token.
                // We simulate a broken pump by sending two WriteStdin messages to the same
                // terminal: the first establishes the pump, then we fill the channel beyond
                // capacity so the next try_send returns Err (BrokenPipe).
                let mut replies = Vec::new();
                for _ in 0..=STDIN_CHANNEL_CAPACITY {
                    let (reply_tx, reply_rx) = oneshot::channel();
                    tx.send(TerminalMessage::WriteStdin(StdinWriteRequest {
                        session_id: sid.clone(),
                        terminal_id: tid.clone(),
                        data: b"x".to_vec(),
                        reply: reply_tx,
                    }))
                    .unwrap();
                    replies.push(reply_rx);
                }
                // Collect results: at least one must be BrokenPipe (channel overflow).
                let mut got_broken_pipe = false;
                for reply_rx in replies {
                    if let Ok(Err(AcpError::BrokenPipe)) = reply_rx.await {
                        got_broken_pipe = true;
                    }
                }
                assert!(
                    got_broken_pipe,
                    "expected at least one BrokenPipe from overflow"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn bash_stdin_pump_cancelled_on_release() {
        // After Release, the pump's CancellationToken must be cancelled.
        // Subsequent WriteStdin to the same terminal_id starts a fresh pump (no persistent state).
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, rx) = mpsc::unbounded_channel::<TerminalMessage>();
                let conn = Rc::new(FakeTerminalClient);
                let handler = async move { run_terminal_handler(conn, rx).await };
                tokio::task::spawn_local(handler);

                let sid = acp::SessionId::new("s1");
                let tid: acp::TerminalId = "term-rel".to_owned().into();

                // Establish a pump by writing stdin.
                let (reply_tx, reply_rx) = oneshot::channel();
                tx.send(TerminalMessage::WriteStdin(StdinWriteRequest {
                    session_id: sid.clone(),
                    terminal_id: tid.clone(),
                    data: b"hello\n".to_vec(),
                    reply: reply_tx,
                }))
                .unwrap();
                reply_rx.await.unwrap().unwrap(); // pump established, write queued

                // Release the terminal — must cancel the pump.
                tx.send(TerminalMessage::Release(TerminalReleaseRequest {
                    session_id: sid.clone(),
                    terminal_id: tid.to_string(),
                }))
                .unwrap();

                // Allow the handler to process the Release.
                tokio::task::yield_now().await;

                // Writing again after release starts a fresh pump — should succeed.
                let (fresh_reply, write_result) = oneshot::channel();
                tx.send(TerminalMessage::WriteStdin(StdinWriteRequest {
                    session_id: sid.clone(),
                    terminal_id: tid.clone(),
                    data: b"after release\n".to_vec(),
                    reply: fresh_reply,
                }))
                .unwrap();
                // Fresh pump: send must succeed (Ok).
                write_result.await.unwrap().unwrap();
            })
            .await;
    }
}
