// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Filesystem/cwd hook dispatch and the MCP dispatch adapter.
//!
//! Extracted from `agent/mod.rs` (#4923). Fires `CwdChanged` and `FileChanged`
//! lifecycle hooks and bridges MCP tool dispatch into `zeph-subagent` via
//! [`McpManagerDispatch`].

use std::sync::Arc;

use super::Agent;
use crate::channel::Channel;

/// Inject `ZEPH_AGENT_TYPE = "main"` and (when present) `ZEPH_AGENT_ID` into a hook
/// environment map. Mirrors how `ZEPH_SESSION_ID` is conditionally inserted at each site.
pub(crate) fn insert_main_agent_ctx(
    env: &mut std::collections::HashMap<String, String>,
    conv_id: Option<&str>,
) {
    env.insert("ZEPH_AGENT_TYPE".to_owned(), "main".to_owned());
    if let Some(id) = conv_id {
        env.insert("ZEPH_AGENT_ID".to_owned(), id.to_owned());
    }
}

impl<C: Channel> Agent<C> {
    /// Return an `McpDispatch` adapter backed by the agent's MCP manager, if present.
    pub(super) fn mcp_dispatch(&self) -> Option<McpManagerDispatch> {
        self.services
            .mcp
            .manager
            .as_ref()
            .map(|m| McpManagerDispatch(Arc::clone(m)))
    }
    /// Check if the process cwd has changed since last call and fire `CwdChanged` hooks.
    ///
    /// Called after each tool batch completes. The check is a single syscall and has
    /// negligible cost. Only fires when cwd actually changed (defense-in-depth: normally
    /// only `set_working_directory` changes cwd; shell child processes cannot affect it).
    #[tracing::instrument(name = "core.agent.check_cwd_changed", skip_all, level = "debug")]
    pub(crate) async fn check_cwd_changed(&mut self) {
        let current = match std::env::current_dir() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("check_cwd_changed: failed to get cwd: {e}");
                return;
            }
        };
        if current == self.runtime.lifecycle.last_known_cwd {
            return;
        }
        let old_cwd =
            std::mem::replace(&mut self.runtime.lifecycle.last_known_cwd, current.clone());
        self.services.session.env_context.working_dir = current.display().to_string();

        tracing::info!(
            old = %old_cwd.display(),
            new = %current.display(),
            "working directory changed"
        );

        self.channel
            .send_status_best_effort("Working directory changed\u{2026}")
            .await;

        let hooks = self.services.session.hooks_config.cwd_changed.clone();
        if hooks.is_empty() {
            tracing::debug!("CwdChanged: no hooks configured, skipping");
        } else {
            tracing::debug!(count = hooks.len(), "CwdChanged: firing hooks");
            let mut env = std::collections::HashMap::new();
            env.insert("ZEPH_OLD_CWD".to_owned(), old_cwd.display().to_string());
            env.insert("ZEPH_NEW_CWD".to_owned(), current.display().to_string());
            let conv_id_str = self
                .services
                .memory
                .persistence
                .conversation_id
                .map(|id| id.0.to_string());
            insert_main_agent_ctx(&mut env, conv_id_str.as_deref());
            let dispatch = self.mcp_dispatch();
            let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
                .as_ref()
                .map(|d| d as &dyn zeph_subagent::McpDispatch);
            if let Err(e) = zeph_subagent::hooks::fire_hooks(&hooks, &env, mcp, None).await {
                tracing::warn!(error = %e, "CwdChanged hook failed");
            } else {
                tracing::info!(count = hooks.len(), "CwdChanged: hooks fired");
            }
        }

        // #6032 FR-003: invalidate the cached repo-map so it regenerates lazily from the new
        // cwd on next prompt assembly (`assembly.rs`) — not gated by safe-mode, repo-map is not
        // a customization source. No eager rebuild here; cheap `Option` clear.
        self.channel
            .send_status_best_effort("Re-scoping repo map\u{2026}")
            .await;
        self.services.index.cached_repo_map = None;

        // #6032 FR-004: re-run CLAUDE.md/AGENTS.md instruction discovery against the new cwd —
        // gated on `!safe_mode` (#6031 S3): a --safe-mode session must never silently re-load
        // project instructions via /cd, which would defeat the flag.
        if self.runtime.config.safe_mode {
            tracing::debug!("safe mode active: skipping instruction re-discovery after cwd change");
        } else if self.runtime.instructions.reload_state.is_some() {
            self.channel
                .send_status_best_effort("Re-discovering project instructions\u{2026}")
                .await;
            if let Some(ref mut state) = self.runtime.instructions.reload_state {
                state.base_dir.clone_from(&current);
            }
            self.reload_instructions().await;
        }

        self.channel.send_status_best_effort("").await;
    }
    /// Handle a `FileChangedEvent` from the file watcher.
    #[tracing::instrument(name = "core.agent.handle_file_changed", skip_all, level = "debug")]
    pub(crate) async fn handle_file_changed(
        &mut self,
        event: crate::file_watcher::FileChangedEvent,
    ) {
        tracing::info!(path = %event.path.display(), "file changed");

        self.channel
            .send_status_best_effort("Running file-change hook\u{2026}")
            .await;

        let hooks = self
            .services
            .session
            .hooks_config
            .file_changed_hooks
            .clone();
        if hooks.is_empty() {
            tracing::debug!(path = %event.path.display(), "FileChanged: no hooks configured, skipping");
        } else {
            tracing::debug!(count = hooks.len(), path = %event.path.display(), "FileChanged: firing hooks");
            let mut env = std::collections::HashMap::new();
            env.insert(
                "ZEPH_CHANGED_PATH".to_owned(),
                event.path.display().to_string(),
            );
            let conv_id_str = self
                .services
                .memory
                .persistence
                .conversation_id
                .map(|id| id.0.to_string());
            insert_main_agent_ctx(&mut env, conv_id_str.as_deref());
            let dispatch = self.mcp_dispatch();
            let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
                .as_ref()
                .map(|d| d as &dyn zeph_subagent::McpDispatch);
            if let Err(e) = zeph_subagent::hooks::fire_hooks(&hooks, &env, mcp, None).await {
                tracing::warn!(error = %e, "FileChanged hook failed");
            } else {
                tracing::info!(count = hooks.len(), path = %event.path.display(), "FileChanged: hooks fired");
            }
        }

        self.channel.send_status_best_effort("").await;
    }
}

/// Thin wrapper that implements [`zeph_subagent::McpDispatch`] over an [`Arc<zeph_mcp::McpManager>`].
///
/// Used to pass MCP tool dispatch capability into `fire_hooks` without coupling
/// `zeph-subagent` to `zeph-mcp`.
pub(super) struct McpManagerDispatch(Arc<zeph_mcp::McpManager>);

impl zeph_subagent::McpDispatch for McpManagerDispatch {
    fn call_tool<'a>(
        &'a self,
        server: &'a str,
        tool: &'a str,
        args: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.0
                .call_tool(server, tool, args)
                .await
                .map(|result| {
                    // Render every content block (text, image, audio, resource, ...) to a
                    // JSON value — non-text blocks no longer silently dropped.
                    let blocks: Vec<serde_json::Value> = result
                        .content
                        .iter()
                        .map(|c| serde_json::Value::String(zeph_mcp::render_content_block(c)))
                        .collect();
                    serde_json::Value::Array(blocks)
                })
                .map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::insert_main_agent_ctx;
    #[allow(clippy::wildcard_imports)]
    use crate::agent::agent_tests::*;

    /// Helper: point the agent's `last_known_cwd` at `from` and the real process cwd at `to`,
    /// so the next `check_cwd_changed()` call detects a change. Restores the original process
    /// cwd on drop-equivalent (explicit restore at the end of each test) — nextest runs each
    /// test in its own process, so this does not race concurrent tests.
    fn simulate_cwd_change(
        agent: &mut Agent<MockChannel>,
        from: &std::path::Path,
        to: &std::path::Path,
    ) {
        agent.runtime.lifecycle.last_known_cwd = from.to_path_buf();
        std::env::set_current_dir(to).unwrap();
    }

    /// #6032 FR-003: repo-map cache invalidation must happen on every cwd change,
    /// unconditionally — not gated by safe-mode (repo-map is not a customization source).
    #[tokio::test]
    async fn check_cwd_changed_invalidates_repo_map_cache() {
        let original_cwd = std::env::current_dir().unwrap();
        let harness = QuickTestAgent::minimal("ok");
        let mut agent = harness.agent;
        agent.services.index.cached_repo_map = Some((
            "<repo_map>stale</repo_map>".to_owned(),
            std::time::Instant::now(),
        ));

        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        simulate_cwd_change(&mut agent, from.path(), to.path());

        agent.check_cwd_changed().await;

        assert!(
            agent.services.index.cached_repo_map.is_none(),
            "cached_repo_map must be cleared after a cwd change"
        );

        let _ = std::env::set_current_dir(&original_cwd);
    }

    /// #6032 FR-004 / #6031 S3: instruction re-discovery must be skipped entirely when the
    /// session is in `--safe-mode` — a `/cd` (or agent-invoked `set_working_directory`) must
    /// never silently re-load CLAUDE.md/AGENTS.md and defeat the flag.
    #[tokio::test]
    async fn check_cwd_changed_skips_instruction_redisovery_when_safe_mode_active() {
        let original_cwd = std::env::current_dir().unwrap();
        let harness = QuickTestAgent::minimal("ok");
        let mut agent = harness.agent;
        agent.runtime.config.safe_mode = true;

        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        std::fs::write(to.path().join("zeph.md"), "# target-dir instructions").unwrap();

        agent.runtime.instructions.reload_state =
            Some(crate::instructions::InstructionReloadState {
                base_dir: from.path().to_path_buf(),
                provider_kinds: vec![crate::config::ProviderKind::Claude],
                explicit_files: Vec::new(),
                auto_detect: false,
            });
        let sentinel_blocks = vec![crate::instructions::InstructionBlock {
            source: from.path().join("zeph.md"),
            content: "# original instructions (must survive)".to_owned(),
        }];
        agent.runtime.instructions.blocks = sentinel_blocks.clone();

        simulate_cwd_change(&mut agent, from.path(), to.path());
        agent.check_cwd_changed().await;

        assert_eq!(
            agent.runtime.instructions.blocks.len(),
            sentinel_blocks.len(),
            "instruction blocks must be unchanged when safe_mode is active"
        );
        assert_eq!(
            agent.runtime.instructions.blocks[0].content, sentinel_blocks[0].content,
            "safe-mode session must not pick up the new directory's zeph.md"
        );

        let _ = std::env::set_current_dir(&original_cwd);
    }

    /// Regression baseline for the test above: outside safe-mode, `/cd` DOES re-discover
    /// instructions from the new directory.
    #[tokio::test]
    async fn check_cwd_changed_rediscovers_instructions_when_not_safe_mode() {
        let original_cwd = std::env::current_dir().unwrap();
        let harness = QuickTestAgent::minimal("ok");
        let mut agent = harness.agent;
        assert!(!agent.runtime.config.safe_mode);

        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        std::fs::write(to.path().join("zeph.md"), "# target-dir instructions").unwrap();

        agent.runtime.instructions.reload_state =
            Some(crate::instructions::InstructionReloadState {
                base_dir: from.path().to_path_buf(),
                provider_kinds: vec![crate::config::ProviderKind::Claude],
                explicit_files: Vec::new(),
                auto_detect: false,
            });
        agent.runtime.instructions.blocks = Vec::new();

        simulate_cwd_change(&mut agent, from.path(), to.path());
        agent.check_cwd_changed().await;

        assert_eq!(
            agent.runtime.instructions.blocks.len(),
            1,
            "instructions must be re-discovered from the new cwd"
        );
        assert!(
            agent.runtime.instructions.blocks[0]
                .content
                .contains("target-dir instructions"),
            "re-discovered block must come from the new directory's zeph.md"
        );

        let _ = std::env::set_current_dir(&original_cwd);
    }

    #[test]
    fn insert_main_agent_ctx_always_sets_agent_type() {
        let mut env = HashMap::new();
        insert_main_agent_ctx(&mut env, None);
        assert_eq!(env.get("ZEPH_AGENT_TYPE").map(String::as_str), Some("main"));
    }

    #[test]
    fn insert_main_agent_ctx_sets_agent_id_when_some() {
        let mut env = HashMap::new();
        insert_main_agent_ctx(&mut env, Some("conv-abc"));
        assert_eq!(
            env.get("ZEPH_AGENT_ID").map(String::as_str),
            Some("conv-abc")
        );
    }

    #[test]
    fn insert_main_agent_ctx_omits_agent_id_when_none() {
        let mut env = HashMap::new();
        insert_main_agent_ctx(&mut env, None);
        assert!(
            !env.contains_key("ZEPH_AGENT_ID"),
            "ZEPH_AGENT_ID must be absent when conv_id is None"
        );
    }
}
