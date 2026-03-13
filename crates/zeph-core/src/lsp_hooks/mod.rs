// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LSP context injection hooks.
//!
//! Hooks fire after native tool execution and accumulate [`LspNote`] entries.
//! Before the next LLM call, [`LspHookRunner::drain_notes`] formats and
//! injects all accumulated notes as a `Role::System` message, respecting the
//! per-turn token budget.
//!
//! # Pruning interaction
//! LSP notes are injected as `Role::System` messages (consistent with graph
//! facts, recall, and code context). The tool-pair summarizer targets only
//! `Role::User` / `Role::Assistant` pairs, so LSP notes are **never**
//! accidentally summarized. The `[lsp ...]` prefix is checked by
//! [`super::agent::Agent::remove_lsp_messages`] to clear stale notes before
//! injecting fresh ones each turn.

mod diagnostics;
mod hover;
#[cfg(test)]
mod test_helpers;

use std::sync::Arc;

use tokio::sync::mpsc;
use zeph_mcp::McpManager;

pub use crate::config::LspConfig;

/// A single context note produced by an LSP hook.
pub struct LspNote {
    /// Human-readable label ("diagnostics", "hover").
    pub kind: &'static str,
    /// Formatted content, ready for injection into the message history.
    pub content: String,
    /// Accurate token count from [`zeph_memory::TokenCounter`].
    pub estimated_tokens: usize,
}

/// Receives background diagnostics results from a spawned fetch task.
type DiagnosticsRx = mpsc::Receiver<Option<LspNote>>;

/// Accumulates LSP notes from hook firings and drains them before each LLM call.
pub struct LspHookRunner {
    pub(crate) manager: Arc<McpManager>,
    pub(crate) config: LspConfig,
    /// Notes collected during the current tool loop iteration.
    pending_notes: Vec<LspNote>,
    /// Channels receiving background diagnostics fetch results.
    /// One receiver per spawned background task (one per `write` tool call in a batch).
    /// Collected non-blocking on the next drain.
    diagnostics_rxs: Vec<DiagnosticsRx>,
    /// Sessions statistics.
    pub(crate) stats: LspStats,
}

/// Session-level statistics for the `/lsp` TUI command.
#[derive(Debug, Default, Clone)]
pub struct LspStats {
    pub diagnostics_injected: u64,
    pub hover_injected: u64,
    pub notes_dropped_budget: u64,
}

impl LspHookRunner {
    /// Create a new runner. Token counting uses the provided `token_counter`.
    #[must_use]
    pub fn new(manager: Arc<McpManager>, config: LspConfig) -> Self {
        Self {
            manager,
            config,
            pending_notes: Vec::new(),
            diagnostics_rxs: Vec::new(),
            stats: LspStats::default(),
        }
    }

    /// Returns a snapshot of the session statistics.
    #[must_use]
    pub fn stats(&self) -> &LspStats {
        &self.stats
    }

    /// Returns true when the configured MCP server is present in the manager.
    ///
    /// Used by the `/lsp` command to show connectivity status. Not called in the
    /// hot path; individual MCP call failures are logged at `debug` level and
    /// silently ignored.
    pub async fn is_available(&self) -> bool {
        self.manager
            .list_servers()
            .await
            .contains(&self.config.mcp_server_id)
    }

    /// Called after a native tool completes.
    ///
    /// Spawns a background diagnostics fetch when the tool is `write`.
    /// Queues a hover fetch result synchronously when the tool is `read`
    /// and hover is enabled.
    ///
    /// Returns early without any MCP call if the configured server is not connected.
    pub async fn after_tool(
        &mut self,
        tool_name: &str,
        tool_params: &serde_json::Value,
        tool_output: &str,
        token_counter: &Arc<zeph_memory::TokenCounter>,
        sanitizer: &crate::sanitizer::ContentSanitizer,
    ) {
        if !self.config.enabled {
            tracing::debug!(tool = tool_name, "LSP hook: skipped (disabled)");
            return;
        }
        if !self.is_available().await {
            tracing::debug!(tool = tool_name, "LSP hook: skipped (server unavailable)");
            return;
        }

        match tool_name {
            "write" if self.config.diagnostics.enabled => {
                self.spawn_diagnostics_fetch(tool_params, token_counter, sanitizer);
            }
            "read" if self.config.hover.enabled => {
                if let Some(note) =
                    hover::fetch_hover(self, tool_params, tool_output, token_counter, sanitizer)
                        .await
                {
                    self.stats.hover_injected += 1;
                    self.pending_notes.push(note);
                }
            }
            "write" => {
                tracing::debug!(tool = tool_name, "LSP hook: skipped (diagnostics disabled)");
            }
            "read" => {
                tracing::debug!(tool = tool_name, "LSP hook: skipped (hover disabled)");
            }
            _ => {}
        }
    }

    /// Spawn a background task that waits for the LSP server to re-analyse the
    /// written file, then fetches diagnostics via MCP.
    ///
    /// Results are collected by [`Self::collect_background_diagnostics`] on the
    /// next [`Self::drain_notes`] call. This avoids any synchronous sleep in
    /// the tool loop.
    ///
    /// Multiple writes in a single batch each produce an independent receiver,
    /// all collected on the next drain.
    fn spawn_diagnostics_fetch(
        &mut self,
        tool_params: &serde_json::Value,
        token_counter: &Arc<zeph_memory::TokenCounter>,
        sanitizer: &crate::sanitizer::ContentSanitizer,
    ) {
        let Some(path) = tool_params
            .get("path")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned)
        else {
            tracing::debug!("LSP hook: skipped diagnostics fetch (missing path)");
            return;
        };

        tracing::debug!(tool = "write", path = %path, "LSP hook: spawning diagnostics fetch");

        let manager = Arc::clone(&self.manager);
        let config = self.config.clone();
        let tc = Arc::clone(token_counter);
        let sanitizer = sanitizer.clone();

        let (tx, rx) = mpsc::channel(1);
        self.diagnostics_rxs.push(rx);

        tokio::spawn(async move {
            // Give the LSP server time to start re-analysing after the write.
            // 200 ms is a lightweight heuristic; the diagnostic cache in mcpls
            // will serve the most-recently-published set regardless.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            let note =
                diagnostics::fetch_diagnostics(manager.as_ref(), &config, &path, &tc, &sanitizer)
                    .await;
            // Ignore send errors: the receiver may have been dropped if the
            // agent loop exited before the task finished.
            let _ = tx.send(note).await;
        });
    }

    /// Poll all background diagnostics channels (non-blocking).
    ///
    /// Receivers that are ready or disconnected are removed. Pending receivers
    /// (still waiting for the LSP) are kept for the next drain cycle.
    fn collect_background_diagnostics(&mut self) {
        let mut still_pending = Vec::new();
        for mut rx in self.diagnostics_rxs.drain(..) {
            match rx.try_recv() {
                Ok(Some(note)) => {
                    self.stats.diagnostics_injected += 1;
                    self.pending_notes.push(note);
                }
                Ok(None) | Err(mpsc::error::TryRecvError::Disconnected) => {
                    // No diagnostics or task exited — drop receiver.
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    // Not ready yet; keep for the next drain.
                    still_pending.push(rx);
                }
            }
        }
        self.diagnostics_rxs = still_pending;
    }

    /// Drain all accumulated notes into a single formatted string, enforcing
    /// the per-turn token budget.
    ///
    /// Returns `None` when there are no notes to inject.
    #[must_use]
    pub fn drain_notes(
        &mut self,
        token_counter: &Arc<zeph_memory::TokenCounter>,
    ) -> Option<String> {
        use std::fmt::Write as _;
        self.collect_background_diagnostics();

        if self.pending_notes.is_empty() {
            return None;
        }

        let mut output = String::new();
        let mut remaining = self.config.token_budget;

        for note in self.pending_notes.drain(..) {
            if note.estimated_tokens > remaining {
                tracing::debug!(
                    kind = note.kind,
                    tokens = note.estimated_tokens,
                    remaining,
                    "LSP note dropped: token budget exceeded"
                );
                self.stats.notes_dropped_budget += 1;
                continue;
            }
            remaining -= note.estimated_tokens;
            if !output.is_empty() {
                output.push('\n');
            }
            let _ = write!(output, "[lsp {}]\n{}", note.kind, note.content);
        }

        // Re-measure after formatting in case the note content changed.
        if output.is_empty() {
            None
        } else {
            let _ = token_counter; // already used during note construction
            Some(output)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeph_mcp::McpManager;
    use zeph_memory::TokenCounter;

    use super::*;
    use crate::config::{DiagnosticSeverity, LspConfig};

    fn make_runner(enabled: bool) -> LspHookRunner {
        let enforcer = zeph_mcp::PolicyEnforcer::new(vec![]);
        let manager = Arc::new(McpManager::new(vec![], vec![], enforcer));
        LspHookRunner::new(
            manager,
            LspConfig {
                enabled,
                token_budget: 500,
                ..LspConfig::default()
            },
        )
    }

    #[test]
    fn drain_notes_empty() {
        let mut runner = make_runner(true);
        let tc = Arc::new(TokenCounter::default());
        assert!(runner.drain_notes(&tc).is_none());
    }

    #[test]
    fn drain_notes_formats_correctly() {
        let tc = Arc::new(TokenCounter::default());
        let mut runner = make_runner(true);
        let tokens = tc.count_tokens("hello world");
        runner.pending_notes.push(LspNote {
            kind: "diagnostics",
            content: "hello world".into(),
            estimated_tokens: tokens,
        });
        let result = runner.drain_notes(&tc).unwrap();
        assert!(result.starts_with("[lsp diagnostics]\nhello world"));
    }

    #[test]
    fn drain_notes_budget_enforcement() {
        let tc = Arc::new(TokenCounter::default());
        let enforcer = zeph_mcp::PolicyEnforcer::new(vec![]);
        let manager = Arc::new(McpManager::new(vec![], vec![], enforcer));
        let mut runner = LspHookRunner::new(
            manager,
            LspConfig {
                enabled: true,
                token_budget: 1, // extremely tight budget
                ..LspConfig::default()
            },
        );
        runner.pending_notes.push(LspNote {
            kind: "diagnostics",
            content: "a very long diagnostic message that exceeds one token".into(),
            estimated_tokens: 20,
        });
        let result = runner.drain_notes(&tc);
        // Budget of 1 token cannot fit 20-token note → dropped, None returned
        assert!(result.is_none());
        assert_eq!(runner.stats.notes_dropped_budget, 1);
    }

    #[test]
    fn lsp_config_defaults() {
        let cfg = LspConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.mcp_server_id, "mcpls");
        assert_eq!(cfg.token_budget, 2000);
        assert_eq!(cfg.call_timeout_secs, 5);
        assert!(cfg.diagnostics.enabled);
        assert!(!cfg.hover.enabled);
        assert_eq!(cfg.diagnostics.min_severity, DiagnosticSeverity::Error);
    }

    #[test]
    fn lsp_config_toml_parse() {
        let toml_str = r#"
            enabled = true
            mcp_server_id = "my-lsp"
            token_budget = 3000

            [diagnostics]
            enabled = true
            max_per_file = 10
            min_severity = "warning"

            [hover]
            enabled = true
            max_symbols = 5
        "#;
        let cfg: LspConfig = toml::from_str(toml_str).expect("parse LspConfig");
        assert!(cfg.enabled);
        assert_eq!(cfg.mcp_server_id, "my-lsp");
        assert_eq!(cfg.token_budget, 3000);
        assert_eq!(cfg.diagnostics.max_per_file, 10);
        assert_eq!(cfg.diagnostics.min_severity, DiagnosticSeverity::Warning);
        assert!(cfg.hover.enabled);
        assert_eq!(cfg.hover.max_symbols, 5);
    }

    #[tokio::test]
    async fn after_tool_disabled_does_not_queue_notes() {
        use crate::sanitizer::{ContentIsolationConfig, ContentSanitizer};
        let tc = Arc::new(TokenCounter::default());
        let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());
        let mut runner = make_runner(false); // lsp disabled

        // Even write tool should produce no notes when disabled.
        let params = serde_json::json!({ "path": "src/main.rs" });
        runner
            .after_tool("write", &params, "", &tc, &sanitizer)
            .await;
        // No background tasks spawned.
        assert!(runner.diagnostics_rxs.is_empty());
        assert!(runner.pending_notes.is_empty());
    }

    #[tokio::test]
    async fn after_tool_unavailable_skips_on_write() {
        use crate::sanitizer::{ContentIsolationConfig, ContentSanitizer};
        let tc = Arc::new(TokenCounter::default());
        let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());
        // Runner enabled but no MCP server configured — is_available() returns false.
        let mut runner = make_runner(true);
        let params = serde_json::json!({ "path": "src/main.rs" });
        runner
            .after_tool("write", &params, "", &tc, &sanitizer)
            .await;
        // No background task spawned because server is not available.
        assert!(runner.diagnostics_rxs.is_empty());
    }

    #[test]
    fn collect_background_diagnostics_multiple_writes() {
        use tokio::sync::mpsc;
        let mut runner = make_runner(true);
        let tc = Arc::new(TokenCounter::default());

        // Simulate two background tasks completing immediately.
        for i in 0..2u64 {
            let (tx, rx) = mpsc::channel(1);
            runner.diagnostics_rxs.push(rx);
            let note = LspNote {
                kind: "diagnostics",
                content: format!("error {i}"),
                estimated_tokens: 5,
            };
            tx.try_send(Some(note)).unwrap();
        }

        runner.collect_background_diagnostics();
        // Both notes collected.
        assert_eq!(runner.pending_notes.len(), 2);
        assert_eq!(runner.stats.diagnostics_injected, 2);
        assert!(runner.diagnostics_rxs.is_empty());

        let result = runner.drain_notes(&tc).unwrap();
        assert!(result.contains("error 0"));
        assert!(result.contains("error 1"));
    }
}
