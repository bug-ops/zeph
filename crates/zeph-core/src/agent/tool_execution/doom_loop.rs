// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Doom-loop detection: repeated identical tool output guard.
//!
//! Split out of `tier_loop.rs` — see that module for the orchestration entry point that calls
//! into this check on every native-tool iteration.

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Returns `true` if a doom loop was detected and the caller should break.
    pub(super) async fn check_doom_loop(
        &mut self,
        iteration: usize,
    ) -> Result<bool, crate::agent::error::AgentError> {
        if let Some(last_msg) = self.msg.messages.last() {
            let hash = zeph_agent_tools::doom_loop_hash(&last_msg.content);
            tracing::debug!(
                iteration,
                hash,
                content_len = last_msg.content.len(),
                content_preview = &last_msg.content[..last_msg.content.len().min(120)],
                "doom-loop hash recorded"
            );
            self.tool_orchestrator.push_doom_hash(hash);
            if self.tool_orchestrator.is_doom_loop() {
                tracing::warn!(
                    iteration,
                    hash,
                    content_len = last_msg.content.len(),
                    content_preview = &last_msg.content[..last_msg.content.len().min(200)],
                    "doom-loop detected: {} consecutive identical outputs",
                    crate::agent::DOOM_LOOP_WINDOW
                );
                self.channel
                    .send("Stopping: detected repeated identical tool outputs.")
                    .await?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}
