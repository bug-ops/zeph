// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use acp::Client as _;
use agent_client_protocol as acp;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use zeph_tools::{
    ToolCall, ToolError, ToolOutput,
    executor::deserialize_params,
    registry::{InvocationHint, ToolDef},
};

use crate::{error::AcpError, permission::AcpPermissionGate};

const KILL_GRACE_TIMEOUT: Duration = Duration::from_secs(5);

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

enum TerminalMessage {
    Execute(TerminalRequest),
    Release(TerminalReleaseRequest),
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
    /// Create the executor and the `LocalSet`-side handler future.
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

impl zeph_tools::ToolExecutor for AcpShellExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: "bash".into(),
            description: "Execute a shell command in the IDE terminal".into(),
            schema: schemars::schema_for!(BashParams),
            invocation: InvocationHint::ToolCall,
        }]
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != "bash" {
            return Ok(None);
        }

        let params: BashParams = deserialize_params(&call.params)?;
        let cwd = params.cwd.map(PathBuf::from);

        if let Some(gate) = &self.permission_gate {
            let fields = acp::ToolCallUpdateFields::new()
                .title(call.tool_id.clone())
                .raw_input(serde_json::json!({ "command": params.command }));
            let tool_call = acp::ToolCallUpdate::new(call.tool_id.clone(), fields);
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

        let summary = match result.exit_code {
            Some(0) | None => result.output,
            Some(code) => format!("[exit {code}]\n{}", result.output),
        };

        Ok(Some(ToolOutput {
            tool_name: "bash".to_owned(),
            summary,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: Some(result.terminal_id),
            locations: None,
        }))
    }
}

async fn run_terminal_handler<C>(conn: Rc<C>, mut rx: mpsc::UnboundedReceiver<TerminalMessage>)
where
    C: acp::Client,
{
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
                req.reply.send(result).ok();
            }
            TerminalMessage::Release(req) => {
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
    let kill_req = acp::KillTerminalCommandRequest::new(session_id.clone(), terminal_id.clone());
    conn.kill_terminal_command(kill_req)
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

        async fn kill_terminal_command(
            &self,
            _args: acp::KillTerminalCommandRequest,
        ) -> acp::Result<acp::KillTerminalCommandResponse> {
            Ok(acp::KillTerminalCommandResponse::new())
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
                    tool_id: "bash".to_owned(),
                    params,
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
                    tool_id: "unknown".to_owned(),
                    params: serde_json::Map::new(),
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
            timeout: Duration::from_secs(120),
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

        async fn kill_terminal_command(
            &self,
            _args: acp::KillTerminalCommandRequest,
        ) -> acp::Result<acp::KillTerminalCommandResponse> {
            Ok(acp::KillTerminalCommandResponse::new())
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
                    tool_id: "bash".to_owned(),
                    params,
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

        async fn kill_terminal_command(
            &self,
            _args: acp::KillTerminalCommandRequest,
        ) -> acp::Result<acp::KillTerminalCommandResponse> {
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
                    tool_id: "bash".to_owned(),
                    params,
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
                    if let acp::SessionUpdate::ToolCallUpdate(update) = notif.update {
                        if let Some(meta) = update.meta {
                            if meta.contains_key("terminal_exit") {
                                got_exit = true;
                            }
                        }
                    }
                }
                assert!(got_exit, "expected terminal_exit notification");
            })
            .await;
    }
}
