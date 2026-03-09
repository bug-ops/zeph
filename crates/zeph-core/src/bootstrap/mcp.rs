// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::any::AnyProvider;

use crate::config::Config;

pub fn create_mcp_manager(config: &Config) -> zeph_mcp::McpManager {
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
}

pub async fn create_mcp_registry(
    config: &Config,
    provider: &AnyProvider,
    mcp_tools: &[zeph_mcp::McpTool],
    embedding_model: &str,
) -> Option<zeph_mcp::McpToolRegistry> {
    if !config.memory.semantic.enabled {
        return None;
    }
    match zeph_mcp::McpToolRegistry::new(&config.memory.qdrant_url) {
        Ok(mut reg) => {
            let embed_fn = provider.embed_fn();
            if let Err(e) = reg.sync(mcp_tools, embedding_model, &embed_fn).await {
                tracing::warn!("MCP tool embedding sync failed: {e:#}");
            }
            Some(reg)
        }
        Err(e) => {
            tracing::warn!("MCP tool registry unavailable: {e:#}");
            None
        }
    }
}
