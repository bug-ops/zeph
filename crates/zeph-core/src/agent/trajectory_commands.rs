// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/trajectory` command handler.
//!
//! Operator-only: score, level, and alert data MUST NOT appear in LLM context.

use super::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Handle `/trajectory [status|reset]` and return a user-visible result.
    pub(super) fn handle_trajectory_command_as_string(&mut self, args: &str) -> String {
        let subcmd = args.split_whitespace().next().unwrap_or("status");
        if subcmd == "reset" {
            // S-HIGH-02: reset is operator-only; refuse from ACP/LLM-callable sessions.
            if self.services.security.is_acp_session {
                return "Permission denied: /trajectory reset is operator-only.".to_owned();
            }
            self.services.security.trajectory.reset();
            *self.services.security.trajectory_risk_slot.write() = 0;
            "Trajectory sentinel reset.".to_owned()
        } else {
            let level = self.services.security.trajectory.current_risk();
            let score = self.services.security.trajectory.score_now();
            let turn = self.services.security.trajectory.current_turn();
            let signals = self.services.security.trajectory.signal_count();
            format!(
                "Trajectory: level={level:?}, score={score:.2}, turn={turn}, signals_in_window={signals}"
            )
        }
    }
}
