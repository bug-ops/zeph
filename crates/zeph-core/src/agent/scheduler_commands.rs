// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_tools::executor::ToolCall;

use super::{Agent, error::AgentError};
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Channel-free version of the scheduler list command for use via
    /// [`zeph_commands::traits::agent::AgentAccess`].
    pub(super) async fn handle_scheduler_list_as_string(&mut self) -> Result<String, AgentError> {
        let call = ToolCall {
            tool_id: zeph_common::ToolName::new("list_tasks"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
        };
        match self.tool_executor.execute_tool_call_erased(&call).await {
            Ok(Some(output)) => Ok(output.summary),
            Ok(None) => {
                Ok("Scheduler is not enabled or list_tasks tool is unavailable.".to_owned())
            }
            Err(e) => Ok(format!("Failed to list scheduled tasks: {e}")),
        }
    }
}
