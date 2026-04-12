// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_tools::executor::ToolCall;

use super::{Agent, error::AgentError};
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Dispatch `/scheduler [subcommand]` slash command.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel send fails or the tool executor returns an error.
    pub async fn handle_scheduler_command(&mut self, input: &str) -> Result<(), AgentError> {
        let args = input.strip_prefix("/scheduler").unwrap_or("").trim();

        if args.is_empty() || args == "list" {
            return self.handle_scheduler_list().await;
        }

        self.channel
            .send("Unknown /scheduler subcommand. Available: /scheduler list")
            .await?;
        Ok(())
    }

    /// Channel-free version of [`Self::handle_scheduler_list`] for use via
    /// [`zeph_commands::traits::agent::AgentAccess`].
    pub(super) async fn handle_scheduler_list_as_string(&mut self) -> Result<String, AgentError> {
        let call = ToolCall {
            tool_id: zeph_common::ToolName::new("list_tasks"),
            params: serde_json::Map::new(),
            caller_id: None,
        };
        match self.tool_executor.execute_tool_call_erased(&call).await {
            Ok(Some(output)) => Ok(output.summary),
            Ok(None) => {
                Ok("Scheduler is not enabled or list_tasks tool is unavailable.".to_owned())
            }
            Err(e) => Ok(format!("Failed to list scheduled tasks: {e}")),
        }
    }

    async fn handle_scheduler_list(&mut self) -> Result<(), AgentError> {
        let call = ToolCall {
            tool_id: zeph_common::ToolName::new("list_tasks"),
            params: serde_json::Map::new(),
            caller_id: None,
        };
        match self.tool_executor.execute_tool_call_erased(&call).await {
            Ok(Some(output)) => {
                self.channel.send(&output.summary).await?;
            }
            Ok(None) => {
                self.channel
                    .send("Scheduler is not enabled or list_tasks tool is unavailable.")
                    .await?;
            }
            Err(e) => {
                self.channel
                    .send(&format!("Failed to list scheduled tasks: {e}"))
                    .await?;
            }
        }
        Ok(())
    }
}
