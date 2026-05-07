// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    #[tracing::instrument(name = "core.tool.process_response", skip_all, level = "debug", err)]
    pub(crate) async fn process_response(&mut self) -> Result<(), crate::agent::error::AgentError> {
        self.services.security.flagged_urls.clear();
        self.process_response_native_tools().await
    }
}
