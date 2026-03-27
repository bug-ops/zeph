// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use zeph_llm::any::AnyProvider;
use zeph_memory::QdrantOps;

use crate::bootstrap::VaultCredentialStore;
use crate::config::{Config, OAuthTokenStorage};
use crate::vault::AgeVaultProvider;

pub fn create_mcp_manager(config: &Config, suppress_stderr: bool) -> zeph_mcp::McpManager {
    create_mcp_manager_with_vault(config, suppress_stderr, None)
}

/// Create an `McpManager` with an optional shared vault for OAuth credential stores.
///
/// When `vault` is `Some`, OAuth servers with `token_storage = "vault"` will use
/// `VaultCredentialStore` backed by the provided age vault.
pub fn create_mcp_manager_with_vault(
    config: &Config,
    suppress_stderr: bool,
    vault: Option<&Arc<RwLock<AgeVaultProvider>>>,
) -> zeph_mcp::McpManager {
    let entries: Vec<zeph_mcp::ServerEntry> = config
        .mcp
        .servers
        .iter()
        .map(|s| {
            let transport = build_transport(s, vault);
            zeph_mcp::ServerEntry {
                id: s.id.clone(),
                transport,
                timeout: std::time::Duration::from_secs(s.timeout),
                trust_level: s.trust_level,
                tool_allowlist: s.tool_allowlist.clone(),
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
    let mut manager =
        zeph_mcp::McpManager::new(entries, config.mcp.allowed_commands.clone(), enforcer)
            .with_suppress_stderr(suppress_stderr);

    // Register OAuth credential stores
    for s in &config.mcp.servers {
        let Some(ref oauth) = s.oauth else { continue };
        if !oauth.enabled {
            continue;
        }
        let store: Arc<dyn rmcp::transport::auth::CredentialStore> = match oauth.token_storage {
            OAuthTokenStorage::Vault => {
                if let Some(vault_arc) = vault {
                    Arc::new(VaultCredentialStore::new(&s.id, Arc::clone(vault_arc)))
                } else {
                    tracing::warn!(
                        server_id = s.id,
                        "OAuth token_storage=vault but no vault provided — falling back to memory"
                    );
                    Arc::new(rmcp::transport::auth::InMemoryCredentialStore::new())
                }
            }
            OAuthTokenStorage::Memory => {
                Arc::new(rmcp::transport::auth::InMemoryCredentialStore::new())
            }
        };
        manager = manager.with_oauth_credential_store(s.id.clone(), store);
    }

    manager
}

fn build_transport(
    s: &crate::config::McpServerConfig,
    vault: Option<&Arc<RwLock<AgeVaultProvider>>>,
) -> zeph_mcp::McpTransport {
    if let Some(ref oauth) = s.oauth
        && oauth.enabled
    {
        // OAuth transport: URL required
        let url = s.url.clone().unwrap_or_default();
        return zeph_mcp::McpTransport::OAuth {
            url,
            scopes: oauth.scopes.clone(),
            callback_port: oauth.callback_port,
            client_name: oauth.client_name.clone(),
        };
    }

    if let Some(ref url) = s.url {
        // HTTP transport: resolve vault references in headers
        let resolved_headers: HashMap<String, String> = s
            .headers
            .iter()
            .map(|(k, v)| {
                let resolved = resolve_vault_ref(v, vault);
                (k.clone(), resolved)
            })
            .collect();
        return zeph_mcp::McpTransport::Http {
            url: url.clone(),
            headers: resolved_headers,
        };
    }

    // Stdio transport
    zeph_mcp::McpTransport::Stdio {
        command: s.command.clone().unwrap_or_default(),
        args: s.args.clone(),
        env: s.env.clone(),
    }
}

/// Resolve vault references of the form `${VAULT_KEY}` in a string value.
///
/// Handles both exact references (`"${KEY}"`) and embedded references
/// (`"Bearer ${KEY}"`, `"Token ${A} and ${B}"`). Each `${KEY}` placeholder is
/// replaced with the corresponding vault value.
///
/// This runs at startup time and is acceptable as blocking I/O.
fn resolve_vault_ref(value: &str, vault: Option<&Arc<RwLock<AgeVaultProvider>>>) -> String {
    if !value.contains("${") {
        return value.to_owned();
    }

    let Some(vault_arc) = vault else {
        tracing::warn!(
            "vault reference(s) in '{}' cannot be resolved: no vault configured",
            value
        );
        return value.to_owned();
    };

    let guard = vault_arc.blocking_read();
    let mut result = value.to_owned();
    let mut search_from = 0;

    while let Some(start) = result[search_from..].find("${") {
        let abs_start = search_from + start;
        let after_brace = abs_start + 2; // skip "${"
        if let Some(end_offset) = result[after_brace..].find('}') {
            let key = result[after_brace..after_brace + end_offset].to_owned();
            let placeholder = format!("${{{key}}}");
            if let Some(resolved) = guard.get(&key) {
                result = result.replacen(&placeholder, resolved, 1);
                // Don't advance past the replacement — it may contain further references
                // (unlikely in practice, but safe to handle).
            } else {
                tracing::warn!("vault reference '${{{key}}}' not found in vault");
                // Skip past this placeholder to avoid an infinite loop.
                search_from = abs_start + placeholder.len();
            }
        } else {
            // Malformed "${..." with no closing brace — stop processing.
            break;
        }
    }

    result
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
