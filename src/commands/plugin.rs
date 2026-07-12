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
// `async` is unused when compiled without the `registry` feature (the Search/Get arms'
// `.await` calls are cfg'd out, leaving the fn body synchronous) — the signature must stay
// `async` regardless, since `runner.rs` always `.await`s this call and the feature is a
// caller-invisible build-time choice (M1, critic handoff).
#[allow(clippy::unused_async)]
pub(crate) async fn handle_plugin_command(
    cmd: PluginCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use crate::bootstrap::{load_config_or_default, resolve_config_path};

    let config_file = resolve_config_path(config_path);
    let config = load_config_or_default(&config_file);

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

        PluginCommand::Search { query } => {
            #[cfg(feature = "registry")]
            {
                registry_search(&config, &query).await?;
            }
            #[cfg(not(feature = "registry"))]
            {
                let _ = &query;
                println!(
                    "This zeph build was compiled without the `registry` feature; rebuild \
                     with `--features registry` (or `full`) to use `zeph plugin search`."
                );
            }
        }

        PluginCommand::Get { registry_id } => {
            #[cfg(feature = "registry")]
            {
                registry_get(&config, &mgr, &registry_id).await?;
            }
            #[cfg(not(feature = "registry"))]
            {
                let _ = &registry_id;
                println!(
                    "This zeph build was compiled without the `registry` feature; rebuild \
                     with `--features registry` (or `full`) to use `zeph plugin get`."
                );
            }
        }
    }

    Ok(())
}

/// `zeph plugin search <query>` (spec-045, #5869, FR-003 — shares the search implementation
/// used by `zeph skill search`, NFR-006).
///
/// Prints [`crate::commands::registry_client::REGISTRY_NOT_CONFIGURED_MSG`] and makes zero
/// network calls when `skills.registry.enabled = false` (FR-004, NFR-001). Thin wrapper around
/// [`registry_search_with`] — see that fn's tests for `MockRegistryClient`-driven coverage.
#[cfg(feature = "registry")]
#[tracing::instrument(name = "plugin.registry_search", skip(config), fields(query))]
async fn registry_search(config: &zeph_core::config::Config, query: &str) -> anyhow::Result<()> {
    use crate::commands::registry_client::{
        REGISTRY_NOT_CONFIGURED_MSG, build_registry_client, resolve_registry_token,
    };

    if !config.skills.registry.enabled {
        println!("{REGISTRY_NOT_CONFIGURED_MSG}");
        anyhow::bail!("{REGISTRY_NOT_CONFIGURED_MSG}");
    }

    let token = resolve_registry_token(config).await?;
    let client = build_registry_client(config, token);
    registry_search_with(client.as_ref(), query).await
}

/// Search logic parameterized over a [`zeph_plugins::marketplace::RegistryClient`] — split out
/// of [`registry_search`] so tests can drive it with `MockRegistryClient` without network or a
/// real `Config`/vault (review fix #4).
#[cfg(feature = "registry")]
async fn registry_search_with(
    client: &dyn zeph_plugins::marketplace::RegistryClient,
    query: &str,
) -> anyhow::Result<()> {
    use crate::commands::registry_client::print_search_results;

    let results = client
        .search(query)
        .await
        .map_err(|e| anyhow::anyhow!("registry search failed: {e}"))?;
    print_search_results(&results);
    Ok(())
}

/// `zeph plugin get <registry-id>` (spec-045, #5869, FR-003).
///
/// Fetches the package and, when it contains a `plugin.toml`, routes it through
/// [`zeph_plugins::PluginManager::add`] unchanged (NFR-002 — same manifest validation, MCP
/// allowlist, and injection scan as `zeph plugin add <local-path>`). Fails with a pointer to
/// `zeph skill get` when the fetched package has no `plugin.toml` (a bare skill package).
/// Thin wrapper around [`registry_get_with`] — see that fn's tests for `MockRegistryClient`-
/// driven coverage.
#[cfg(feature = "registry")]
#[tracing::instrument(name = "plugin.registry_get", skip(config, mgr), fields(registry_id))]
async fn registry_get(
    config: &zeph_core::config::Config,
    mgr: &zeph_plugins::PluginManager,
    registry_id: &str,
) -> anyhow::Result<()> {
    use crate::commands::registry_client::{
        REGISTRY_NOT_CONFIGURED_MSG, build_registry_client, resolve_registry_token,
    };

    if !config.skills.registry.enabled {
        println!("{REGISTRY_NOT_CONFIGURED_MSG}");
        anyhow::bail!("{REGISTRY_NOT_CONFIGURED_MSG}");
    }

    let token = resolve_registry_token(config).await?;
    let client = build_registry_client(config, token);
    registry_get_with(client.as_ref(), mgr, registry_id).await
}

/// Fetch-and-install logic parameterized over a [`zeph_plugins::marketplace::RegistryClient`] —
/// split out of [`registry_get`] so tests can drive it with `MockRegistryClient` (review fix #4).
#[cfg(feature = "registry")]
async fn registry_get_with(
    client: &dyn zeph_plugins::marketplace::RegistryClient,
    mgr: &zeph_plugins::PluginManager,
    registry_id: &str,
) -> anyhow::Result<()> {
    let archive = client
        .fetch(registry_id)
        .await
        .map_err(|e| anyhow::anyhow!("registry fetch failed: {e}"))?;

    if !archive.has_plugin_manifest {
        anyhow::bail!(
            "package {registry_id:?} is a bare skill package (no plugin.toml); use \
             `zeph skill get {registry_id}` instead"
        );
    }

    let source = archive
        .install_dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("registry temp dir path is not valid UTF-8"))?;
    let result = mgr.add(source)?;
    println!(
        "Installed plugin \"{}\" from registry {registry_id:?}.",
        result.name
    );
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

    Ok(())
}

#[cfg(all(test, feature = "registry"))]
mod registry_tests {
    use super::*;
    use zeph_plugins::marketplace::RegistryEntry;
    use zeph_plugins::marketplace::mock::MockRegistryClient;

    fn sample_entry(id: &str, name: &str) -> RegistryEntry {
        RegistryEntry {
            registry_id: id.to_owned(),
            name: name.to_owned(),
            description: "a test plugin".to_owned(),
            tags: vec![],
            author: None,
            security_audit_status: None,
        }
    }

    fn test_manager(root: &std::path::Path) -> zeph_plugins::PluginManager {
        zeph_plugins::PluginManager::new(
            root.join("plugins"),
            root.join("managed-skills"),
            Vec::new(),
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn registry_search_with_calls_client_and_succeeds() {
        let mock = MockRegistryClient::new().with_entry(sample_entry("acme/x", "X Plugin"));
        registry_search_with(&mock, "x").await.unwrap();
    }

    #[tokio::test]
    async fn registry_search_with_propagates_client_error() {
        let mock = MockRegistryClient::new().failing("boom");
        let err = registry_search_with(&mock, "x").await.unwrap_err();
        assert!(err.to_string().contains("registry search failed"));
    }

    #[tokio::test]
    async fn registry_get_with_installs_plugin_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = test_manager(dir.path());

        let mock = MockRegistryClient::new().with_package(
            "acme/full-plugin",
            vec![
                (
                    "plugin.toml".to_owned(),
                    "[plugin]\nname = \"full-plugin\"\nversion = \"0.1.0\"".to_owned(),
                ),
                (
                    "skills/x/SKILL.md".to_owned(),
                    "---\nname: x\ndescription: a test skill\n---\nbody".to_owned(),
                ),
            ],
        );

        registry_get_with(&mock, &mgr, "acme/full-plugin")
            .await
            .unwrap();

        assert!(
            dir.path()
                .join("plugins/full-plugin/.plugin.toml")
                .is_file()
        );
    }

    #[tokio::test]
    async fn registry_get_with_rejects_bare_skill_package_with_pointer_to_skill_get() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = test_manager(dir.path());

        let mock = MockRegistryClient::new().with_package(
            "acme/x",
            vec![(
                "SKILL.md".to_owned(),
                "---\nname: x\ndescription: a test skill\n---\nbody".to_owned(),
            )],
        );

        let err = registry_get_with(&mock, &mgr, "acme/x").await.unwrap_err();
        assert!(err.to_string().contains("zeph skill get acme/x"));
    }

    #[tokio::test]
    async fn registry_get_with_propagates_fetch_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = test_manager(dir.path());
        let mock = MockRegistryClient::new();

        let err = registry_get_with(&mock, &mgr, "missing/id")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("registry fetch failed"));
    }

    // ── #5943: disabled-registry bail must match the printed FR-004 message ──
    //
    // Regression coverage for the outer `registry_search`/`registry_get` gate (not exercised by
    // the `_with` tests above, which only drive post-gate logic): before the fix, the bail! used
    // a hardcoded `"skill registry is not configured"` string — wrong wording for a plugin
    // subcommand too — that diverged from the `REGISTRY_NOT_CONFIGURED_MSG` printed to stdout
    // just above it. Asserting exact equality (not just `contains`) pins both wording and future
    // re-divergence.

    #[tokio::test]
    async fn registry_search_bails_with_registry_not_configured_msg_when_disabled() {
        use crate::commands::registry_client::REGISTRY_NOT_CONFIGURED_MSG;

        let config = zeph_core::config::Config::default();
        assert!(!config.skills.registry.enabled);

        let err = registry_search(&config, "query").await.unwrap_err();
        assert_eq!(err.to_string(), REGISTRY_NOT_CONFIGURED_MSG);
    }

    #[tokio::test]
    async fn registry_get_bails_with_registry_not_configured_msg_when_disabled() {
        use crate::commands::registry_client::REGISTRY_NOT_CONFIGURED_MSG;

        let dir = tempfile::tempdir().unwrap();
        let mgr = test_manager(dir.path());

        let config = zeph_core::config::Config::default();
        assert!(!config.skills.registry.enabled);

        let err = registry_get(&config, &mgr, "acme/x").await.unwrap_err();
        assert_eq!(err.to_string(), REGISTRY_NOT_CONFIGURED_MSG);
    }
}
