// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use dialoguer::{Confirm, Input, Select};
use zeph_core::config::{McpOAuthConfig, McpServerConfig, McpTrustLevel, OAuthTokenStorage};

use super::WizardState;

#[allow(clippy::too_many_lines)]
pub(super) fn step_mcpls(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== MCP: LSP Code Intelligence ==\n");

    // Detect mcpls by searching PATH — avoids spawning a process that could hang.
    let detected = mcpls_in_path();

    if detected {
        println!("mcpls detected.");
    } else {
        println!("mcpls not found. Install with: cargo install mcpls");
    }

    state.mcpls_enabled = Confirm::new()
        .with_prompt("Enable LSP code intelligence via mcpls?")
        .default(detected)
        .interact()?;

    if state.mcpls_enabled {
        let roots_raw: String = Input::new()
            .with_prompt(
                "Workspace root paths (comma-separated, leave empty for current directory)",
            )
            .default(String::new())
            .interact_text()?;
        state.mcpls_workspace_roots = roots_raw
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        // mcpls auto-detects language servers from project files (Cargo.toml, pyproject.toml,
        // tsconfig.json, go.mod). No language selection is needed at wizard time.
    }

    println!();
    Ok(())
}

pub(super) fn mcpls_in_path() -> bool {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let exe_name = if cfg!(windows) { "mcpls.exe" } else { "mcpls" };
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(exe_name))
        .any(|p| p.is_file())
}

pub(super) fn write_mcpls_config(
    state: &WizardState,
    config_path: &std::path::Path,
) -> anyhow::Result<()> {
    let base = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let zeph_dir = base.join(".zeph");
    std::fs::create_dir_all(&zeph_dir)?;

    let roots = if state.mcpls_workspace_roots.is_empty() {
        vec![".".to_owned()]
    } else {
        state.mcpls_workspace_roots.clone()
    };

    let roots_toml = roots
        .iter()
        .map(|r| format!("\"{}\"", r.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");

    // Include explicit language_extensions to work around mcpls serde default Vec bug
    // where [workspace] with only `roots` results in an empty extension map.
    let content = format!(
        r#"[workspace]
roots = [{roots_toml}]

[[workspace.language_extensions]]
language_id = "rust"
extensions = ["rs"]

[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
"#
    );

    let mcpls_path = zeph_dir.join("mcpls.toml");
    zeph_common::fs_secure::write_private(&mcpls_path, content.as_bytes())?;
    println!("mcpls config written to {}", mcpls_path.display());

    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) fn step_mcp_remote(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== MCP: Remote Servers ==\n");
    println!(
        "Configure remote MCP servers that require authentication (static headers or OAuth 2.1)."
    );
    println!("Skip this step if you have no remote MCP servers.\n");

    loop {
        let add = Confirm::new()
            .with_prompt("Add a remote MCP server?")
            .default(false)
            .interact()?;
        if !add {
            break;
        }

        let id: String = Input::new()
            .with_prompt("Server ID (unique slug, e.g. 'todoist')")
            .interact_text()?;
        let url: String = Input::new()
            .with_prompt("Server URL (e.g. https://mcp.example.com)")
            .interact_text()?;

        let auth_choices = [
            "None (no auth)",
            "Static header (Bearer token)",
            "OAuth 2.1 (interactive flow)",
        ];
        let auth_sel = Select::new()
            .with_prompt("Authentication method")
            .items(auth_choices)
            .default(0)
            .interact()?;

        let mut headers = std::collections::HashMap::new();
        let mut oauth: Option<McpOAuthConfig> = None;

        match auth_sel {
            1 => {
                println!("Header value supports vault references: ${{VAULT_KEY}}");
                let header_name: String = Input::new()
                    .with_prompt("Header name")
                    .default("Authorization".into())
                    .interact_text()?;
                let header_value: String = Input::new()
                    .with_prompt("Header value (e.g. 'Bearer ${{MY_TOKEN}}')")
                    .interact_text()?;
                headers.insert(header_name, header_value);
            }
            2 => {
                let storage_choices =
                    ["vault (persisted in age vault)", "memory (lost on restart)"];
                let storage_sel = Select::new()
                    .with_prompt("Token storage")
                    .items(storage_choices)
                    .default(0)
                    .interact()?;
                let token_storage = if storage_sel == 0 {
                    OAuthTokenStorage::Vault
                } else {
                    OAuthTokenStorage::Memory
                };
                let scopes_raw: String = Input::new()
                    .with_prompt("OAuth scopes (space-separated, leave empty for server default)")
                    .default(String::new())
                    .interact_text()?;
                let scopes: Vec<String> =
                    scopes_raw.split_whitespace().map(str::to_owned).collect();
                let callback_port: u16 = Input::new()
                    .with_prompt("Local callback port (0 = auto-assign)")
                    .default(18766)
                    .interact_text()?;
                let client_name: String = Input::new()
                    .with_prompt("OAuth client name")
                    .default("Zeph".into())
                    .interact_text()?;
                oauth = Some(McpOAuthConfig {
                    enabled: true,
                    token_storage,
                    scopes,
                    callback_port,
                    client_name,
                });
            }
            _ => {}
        }

        let trust_choices = ["untrusted (default)", "trusted", "sandboxed"];
        let trust_idx = Select::new()
            .with_prompt("Trust level")
            .items(trust_choices)
            .default(0)
            .interact()?;
        let trust_level = match trust_idx {
            1 => McpTrustLevel::Trusted,
            2 => McpTrustLevel::Sandboxed,
            _ => McpTrustLevel::Untrusted,
        };

        state.mcp_remote_servers.push(McpServerConfig {
            id,
            command: None,
            args: Vec::new(),
            env: std::collections::HashMap::new(),
            url: Some(url),
            timeout: 30,
            policy: zeph_config::McpPolicy::default(),
            headers,
            oauth,
            trust_level,
            tool_allowlist: None,
            expected_tools: Vec::new(),
            roots: Vec::new(),
            tool_metadata: std::collections::HashMap::new(),
            elicitation_enabled: None,
            env_isolation: None,
        });

        println!("Server added.");
    }

    println!();
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) fn step_mcp_discovery(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== MCP: Tool Discovery ==\n");
    println!("Controls how MCP tools are selected per turn when you have many tools configured.");
    println!("  none      — all tools passed to the LLM every turn (default, safest)");
    println!("  embedding — cosine similarity via embedding; fast, no extra LLM call per turn");
    println!("  llm       — LLM-based pruning via mcp.pruning config\n");

    let strategy_choices = ["none", "embedding", "llm"];
    let default_idx = match state.mcp_discovery_strategy.as_str() {
        "embedding" => 1,
        "llm" => 2,
        _ => 0,
    };
    let idx = Select::new()
        .with_prompt("MCP tool discovery strategy")
        .items(strategy_choices)
        .default(default_idx)
        .interact()?;
    strategy_choices[idx].clone_into(&mut state.mcp_discovery_strategy);

    if state.mcp_discovery_strategy == "embedding" {
        let top_k: usize = Input::new()
            .with_prompt("Max tools to select per turn (top_k)")
            .default(10)
            .interact_text()?;
        state.mcp_discovery_top_k = top_k;

        let provider: String = Input::new()
            .with_prompt("Embedding provider name from [[llm.providers]] (leave empty for default)")
            .default(String::new())
            .interact_text()?;
        state.mcp_discovery_provider = provider;
    }

    println!();
    Ok(())
}
