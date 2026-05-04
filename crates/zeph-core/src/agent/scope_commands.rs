// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/scope` command handler.

use super::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Handle `/scope [list|reset]` and return a user-visible result.
    pub(super) fn handle_scope_command_as_string(&self, args: &str) -> String {
        let mut parts = args.split_whitespace();
        let subcmd = parts.next().unwrap_or("list");
        match subcmd {
            "list" => {
                let cfg = &self.runtime.config.security.capability_scopes;
                if cfg.scopes.is_empty() {
                    return "No capability scopes configured.".to_owned();
                }
                let mut out = String::from("Capability scopes:\n");
                for (name, scope) in &cfg.scopes {
                    use std::fmt::Write as _;
                    let _ = writeln!(out, "  [{name}] patterns={}", scope.patterns.len());
                }
                out
            }
            "reset" => {
                // F6: reset active scope to the configured default.
                let default = self
                    .runtime
                    .config
                    .security
                    .capability_scopes
                    .default_scope
                    .clone();
                format!(
                    "Scope reset to default ('{default}'). Use ScopedToolExecutor::set_scope_for_task at runtime to apply."
                )
            }
            other => format!("Unknown /scope subcommand: {other}. Use: list, reset"),
        }
    }
}
