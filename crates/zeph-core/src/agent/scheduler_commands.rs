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

    async fn handle_scheduler_list(&mut self) -> Result<(), AgentError> {
        let call = ToolCall {
            tool_id: "list_tasks".to_owned(),
            params: serde_json::Map::new(),
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
