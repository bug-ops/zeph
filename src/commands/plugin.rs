// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::cli::PluginCommand;

/// Prints the resolved overlay summary for the plugins directory.
///
/// Displays contributing and skipped plugins accurately. Does NOT show the
/// post-intersection merged `allowed_commands` values — those depend on the
/// live `Config` base, which is not available here. Users who want the merged
/// live values should inspect `tools.shell.allowed_commands` in `config.toml`
/// after startup (logged at INFO level on first reload).
fn print_overlay_section(plugins_dir: &std::path::Path) -> anyhow::Result<()> {
    let mut cfg = zeph_core::config::Config::default();
    let overlay = zeph_plugins::apply_plugin_config_overlays(&mut cfg, plugins_dir)
        .map_err(|e| anyhow::anyhow!("overlay resolution failed: {e}"))?;

    if overlay.source_plugins.is_empty() && overlay.skipped_plugins.is_empty() {
        println!("No plugin overlay active.");
        return Ok(());
    }

    println!("Active plugin overlay:");

    if overlay.source_plugins.is_empty() {
        println!("  Source plugins:  (none)");
    } else {
        println!("  Source plugins:  {}", overlay.source_plugins.join(", "));
    }

    if overlay.skipped_plugins.is_empty() {
        println!("  Skipped plugins: (none)");
    } else {
        println!("  Skipped plugins:");
        for reason in &overlay.skipped_plugins {
            println!("    - {reason}");
        }
    }

    println!(
        "  Note: overlay values shown against default config — run with --config for live intersection."
    );

    Ok(())
}

/// Handle `zeph plugin` subcommands.
///
/// # Errors
///
/// Returns an error if the plugin operation fails (invalid manifest, conflicts, etc.).
pub(crate) fn handle_plugin_command(
    cmd: PluginCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use crate::bootstrap::resolve_config_path;

    let config_file = resolve_config_path(config_path);
    let config = zeph_core::config::Config::load(&config_file).unwrap_or_default();

    let plugins_dir = crate::bootstrap::plugins_dir();
    std::fs::create_dir_all(&plugins_dir)
        .map_err(|e| anyhow::anyhow!("failed to create plugins dir: {e}"))?;

    let managed_skills_dir = crate::bootstrap::managed_skills_dir();
    let mcp_allowed = config.mcp.allowed_commands.clone();
    let base_shell_allowed = config.tools.shell.allowed_commands.clone();

    let mgr = zeph_plugins::PluginManager::new(
        plugins_dir.clone(),
        managed_skills_dir,
        mcp_allowed,
        base_shell_allowed,
    );

    match cmd {
        PluginCommand::List { overlay } => {
            if overlay {
                print_overlay_section(&plugins_dir)?;
            } else {
                let installed = mgr.list_installed()?;
                if installed.is_empty() {
                    println!("No plugins installed.");
                } else {
                    for p in &installed {
                        println!("{} v{} — {}", p.name, p.version, p.description);
                    }
                }
            }
        }

        PluginCommand::Add { source } => {
            let result = mgr.add(&source)?;
            println!("Installed plugin \"{}\".", result.name);
            if !result.installed_skills.is_empty() {
                println!("  Skills: {}", result.installed_skills.join(", "));
            }
            if !result.mcp_server_ids.is_empty() {
                println!(
                    "  MCP servers (restart required): {}",
                    result.mcp_server_ids.join(", ")
                );
            }
            for w in &result.warnings {
                eprintln!("warning: {w}");
            }
            // Pointer to plugin add for future users.
            println!(
                "\nPlugins are managed separately. Run `zeph plugin add <source>` to install more."
            );
        }

        PluginCommand::Remove { name } => {
            let result = mgr.remove(&name)?;
            println!("Removed plugin \"{name}\".");
            if !result.removed_skills.is_empty() {
                println!("  Removed skills: {}", result.removed_skills.join(", "));
            }
            if !result.removed_mcp_ids.is_empty() {
                println!(
                    "  MCP servers removed (restart required): {}",
                    result.removed_mcp_ids.join(", ")
                );
            }
        }
    }

    Ok(())
}
