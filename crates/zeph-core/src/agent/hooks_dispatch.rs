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

        let _ = self
            .channel
            .send_status("Working directory changed\u{2026}")
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

        let _ = self.channel.send_status("").await;
    }
    /// Handle a `FileChangedEvent` from the file watcher.
    #[tracing::instrument(name = "core.agent.handle_file_changed", skip_all, level = "debug")]
    pub(crate) async fn handle_file_changed(
        &mut self,
        event: crate::file_watcher::FileChangedEvent,
    ) {
        tracing::info!(path = %event.path.display(), "file changed");

        let _ = self
            .channel
            .send_status("Running file-change hook\u{2026}")
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

        let _ = self.channel.send_status("").await;
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
