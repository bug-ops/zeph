// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;

use super::{Agent, error::AgentError};
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Channel-free version for use via [`zeph_commands::traits::agent::AgentAccess`].
    pub(super) async fn handle_lsp_status_as_string(&mut self) -> Result<String, AgentError> {
        let mut out = String::new();

        match self.session.lsp_hooks.as_ref() {
            None => {
                let _ = writeln!(out, "LSP context injection: disabled");
                let _ = writeln!(out);
                let _ = writeln!(
                    out,
                    "Enable with `--lsp-context` flag or set `lsp.enabled = true` in config."
                );
                let _ = writeln!(
                    out,
                    "Requires mcpls configured under [mcp.servers] (install: cargo install mcpls)."
                );
            }
            Some(lsp) => {
                let available = lsp.is_available().await;
                let _ = writeln!(out, "LSP context injection: enabled");
                let _ = writeln!(
                    out,
                    "MCP server: {} ({})",
                    lsp.config.mcp_server_id,
                    if available {
                        "connected"
                    } else {
                        "not connected"
                    }
                );
                let _ = writeln!(out, "Token budget per turn: {}", lsp.config.token_budget);
                let _ = writeln!(out);
                let _ = writeln!(out, "Hooks:");
                let _ = writeln!(
                    out,
                    "  diagnostics-on-save: {}",
                    if lsp.config.diagnostics.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                let _ = writeln!(
                    out,
                    "  hover-on-read:       {}",
                    if lsp.config.hover.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                let _ = writeln!(out);
                let stats = lsp.stats();
                let _ = writeln!(out, "Session statistics:");
                let _ = writeln!(
                    out,
                    "  diagnostics injected: {}",
                    stats.diagnostics_injected
                );
                let _ = writeln!(out, "  hover injected:       {}", stats.hover_injected);
                let _ = writeln!(
                    out,
                    "  notes dropped (budget): {}",
                    stats.notes_dropped_budget
                );
            }
        }

        Ok(out.trim_end().to_owned())
    }
}
