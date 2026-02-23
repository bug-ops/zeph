use std::rc::Rc;

use tokio::task::LocalSet;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::agent::ZephAcpAgent;
use crate::error::AcpError;

/// Run the ACP server over stdin/stdout until the connection closes.
///
/// Spawns an `AgentSideConnection` on a `LocalSet` (required because the ACP SDK
/// uses non-`Send` futures internally via `async_trait(?Send)`).
///
/// # Errors
///
/// Returns [`AcpError::Transport`] if the I/O future fails.
pub async fn serve_stdio(agent: ZephAcpAgent) -> Result<(), AcpError> {
    let local = LocalSet::new();
    let agent = Rc::new(agent);

    local
        .run_until(async move {
            let stdin = tokio::io::stdin().compat();
            let stdout = tokio::io::stdout().compat_write();

            let agent_ref = Rc::clone(&agent);
            let (conn, io_fut) =
                agent_client_protocol::AgentSideConnection::new(agent, stdout, stdin, |fut| {
                    tokio::task::spawn_local(fut);
                });

            agent_ref.set_connection(conn).await;

            io_fut.await.map_err(|e| AcpError::Transport(e.to_string()))
        })
        .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn transport_module_compiles() {
        // TODO: add functional transport tests with mock stdio
    }
}
