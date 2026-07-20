// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`zeph_commands::SchedulerAccess`] implementation for [`Agent<C>`]: `/scheduler`.
//!
//! [`Agent<C>`]: super::Agent

use std::future::Future;
use std::pin::Pin;

use zeph_commands::{CommandError, SchedulerAccess};

use super::Agent;
use crate::channel::Channel;

#[cfg(feature = "scheduler")]
impl<C: Channel> Agent<C> {
    /// Channel-free version of the scheduler list command for use via
    /// [`zeph_commands::SchedulerAccess`].
    pub(super) async fn handle_scheduler_list_as_string(
        &mut self,
    ) -> Result<String, super::error::AgentError> {
        use zeph_tools::executor::ToolCall;

        let call = ToolCall {
            tool_id: zeph_common::ToolName::new("list_tasks"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
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

impl<C: Channel + Send + 'static> SchedulerAccess for Agent<C> {
    // ----- /scheduler -----

    #[cfg(feature = "scheduler")]
    fn list_scheduled_tasks<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            let result = self
                .handle_scheduler_list_as_string()
                .await
                .map_err(|e| CommandError::new(e.to_string()))?;
            Ok(Some(result))
        })
    }

    #[cfg(not(feature = "scheduler"))]
    fn list_scheduled_tasks<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>> {
        Box::pin(async move { Ok(None) })
    }
}
