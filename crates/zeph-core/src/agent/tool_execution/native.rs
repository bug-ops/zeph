// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    #[tracing::instrument(name = "core.tool.process_response", skip_all, level = "debug", err)]
    pub(crate) async fn process_response(&mut self) -> Result<(), crate::agent::error::AgentError> {
        self.services.security.flagged_urls.clear();
        // Self-reflection re-enters this method from inside a tool batch; the outer loop's
        // frozen set must survive the nested call.
        let outer_turn_set = self.services.tool_state.turn_tool_set.take();
        let result = self.process_response_native_tools().await;
        self.services.tool_state.turn_tool_set = outer_turn_set;
        result
    }
}
