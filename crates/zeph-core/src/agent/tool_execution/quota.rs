// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session tool-call quota gate.
//!
//! Split out of `tier_loop.rs` — see that module for the orchestration entry point
//! (`prepare_tool_dispatch`) that calls into this gate.

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    pub(super) fn check_and_update_quota(&mut self, batch_len: usize) -> bool {
        if let Some(max) = self.tool_orchestrator.check_quota() {
            tracing::warn!(
                max,
                count = self.tool_orchestrator.session_tool_call_count,
                "tool call quota exceeded for session"
            );
            return true;
        }
        let batch_count = u32::try_from(batch_len).unwrap_or(u32::MAX);
        self.tool_orchestrator.session_tool_call_count = self
            .tool_orchestrator
            .session_tool_call_count
            .saturating_add(batch_count);
        self.runtime.lifecycle.turn_tool_calls = self
            .runtime
            .lifecycle
            .turn_tool_calls
            .saturating_add(batch_count);
        false
    }
}
