// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::any::AnyProvider;
use zeph_memory::QdrantOps;

use crate::config::Config;

pub fn create_mcp_manager(config: &Config, suppress_stderr: bool) -> zeph_mcp::McpManager {
    let entries: Vec<zeph_mcp::ServerEntry> = config
        .mcp
        .servers
        .iter()
        .map(|s| {
            let transport = if let Some(ref url) = s.url {
                zeph_mcp::McpTransport::Http { url: url.clone() }
            } else {
                zeph_mcp::McpTransport::Stdio {
                    command: s.command.clone().unwrap_or_default(),
                    args: s.args.clone(),
                    env: s.env.clone(),
                }
            };
            zeph_mcp::ServerEntry {
                id: s.id.clone(),
                transport,
                timeout: std::time::Duration::from_secs(s.timeout),
                trusted: true,
            }
        })
        .collect();
    let policy_entries: Vec<(String, zeph_mcp::McpPolicy)> = config
        .mcp
        .servers
        .iter()
        .map(|s| (s.id.clone(), s.policy.clone()))
        .collect();
    let enforcer = zeph_mcp::PolicyEnforcer::new(policy_entries);
    zeph_mcp::McpManager::new(entries, config.mcp.allowed_commands.clone(), enforcer)
        .with_suppress_stderr(suppress_stderr)
}

pub async fn create_mcp_registry(
    config: &Config,
    provider: &AnyProvider,
    mcp_tools: &[zeph_mcp::McpTool],
    embedding_model: &str,
    qdrant_ops: Option<&QdrantOps>,
) -> Option<zeph_mcp::McpToolRegistry> {
    if !config.memory.semantic.enabled {
        return None;
    }
    let Some(ops) = qdrant_ops else {
        tracing::debug!("MCP tool registry skipped: no Qdrant backend configured");
        return None;
    };
    let mut reg = zeph_mcp::McpToolRegistry::with_ops(ops.clone());
    let embed_fn = provider.embed_fn();
    if let Err(e) = reg.sync(mcp_tools, embedding_model, &embed_fn).await {
        tracing::warn!("MCP tool embedding sync failed: {e:#}");
    }
    Some(reg)
}
