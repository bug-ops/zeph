use std::path::PathBuf;
use std::rc::Rc;

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

struct ShellResult {
    output: String,
    exit_code: Option<u32>,
}

struct TerminalRequest {
    session_id: acp::SessionId,
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    reply: oneshot::Sender<Result<ShellResult, AcpError>>,
}

/// IDE-proxied shell executor.
///
/// Routes `bash` tool calls to the IDE terminal via ACP `terminal/*` methods.
/// Only constructed when the IDE advertises `terminal` capability.
#[derive(Clone)]
pub struct AcpShellExecutor {
    session_id: acp::SessionId,
    request_tx: mpsc::UnboundedSender<TerminalRequest>,
    permission_gate: Option<AcpPermissionGate>,
}

impl AcpShellExecutor {
    /// Create the executor and the `LocalSet`-side handler future.
    pub fn new<C>(
        conn: Rc<C>,
        session_id: acp::SessionId,
        permission_gate: Option<AcpPermissionGate>,
    ) -> (Self, impl std::future::Future<Output = ()>)
    where
        C: acp::Client + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel::<TerminalRequest>();
        let handler = async move { run_terminal_handler(conn, rx).await };
        (
            Self {
                session_id,
                request_tx: tx,
                permission_gate,
            },
            handler,
        )
    }

    async fn execute_shell(
        &self,
        command: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
    ) -> Result<ShellResult, AcpError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.request_tx
            .send(TerminalRequest {
                session_id: self.session_id.clone(),
                command,
                args,
                cwd,
                reply: reply_tx,
            })
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
            id: "bash",
            description: "Execute a shell command in the IDE terminal",
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
            let fields = acp::ToolCallUpdateFields::new().title(params.command.clone());
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
            .execute_shell(params.command, params.args, cwd)
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
        }))
    }
}

async fn run_terminal_handler<C>(conn: Rc<C>, mut rx: mpsc::UnboundedReceiver<TerminalRequest>)
where
    C: acp::Client,
{
    while let Some(req) = rx.recv().await {
        let result =
            execute_in_terminal(&conn, req.session_id, req.command, req.args, req.cwd).await;
        req.reply.send(result).ok();
    }
}

async fn execute_in_terminal<C>(
    conn: &Rc<C>,
    session_id: acp::SessionId,
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
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

    // 2. Wait for exit.
    let wait_req = acp::WaitForTerminalExitRequest::new(session_id.clone(), terminal_id.clone());
    let wait_resp = conn
        .wait_for_terminal_exit(wait_req)
        .await
        .map_err(|e| AcpError::ClientError(e.to_string()))?;
    let exit_code = wait_resp.exit_status.exit_code;

    // 3. Get final output.
    let output_req = acp::TerminalOutputRequest::new(session_id.clone(), terminal_id.clone());
    let output_resp = conn
        .terminal_output(output_req)
        .await
        .map_err(|e| AcpError::ClientError(e.to_string()))?;

    // 4. Release terminal.
    let release_req = acp::ReleaseTerminalRequest::new(session_id, terminal_id);
    conn.release_terminal(release_req)
        .await
        .map_err(|e| AcpError::ClientError(e.to_string()))?;

    Ok(ShellResult {
        output: output_resp.output,
        exit_code,
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
                let (exec, handler) = AcpShellExecutor::new(conn, sid, None);
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
                let (exec, handler) = AcpShellExecutor::new(conn, sid, None);
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
        let (tx, _rx) = mpsc::unbounded_channel::<TerminalRequest>();
        let exec = AcpShellExecutor {
            session_id: acp::SessionId::new("s"),
            request_tx: tx,
            permission_gate: None,
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
                let (exec, handler) = AcpShellExecutor::new(conn, sid, None);
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
                let (gate, perm_handler) = AcpPermissionGate::new(perm_conn, None);
                tokio::task::spawn_local(perm_handler);

                let term_conn = Rc::new(FakeTerminalClient);
                let (exec, term_handler) = AcpShellExecutor::new(term_conn, sid, Some(gate));
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
}
