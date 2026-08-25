// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use tracing::Instrument as _;
use zeph_commands::{CommandError, IntegrationAccess};

use super::command_macros::delegate_cmd;
use super::{Agent, error::AgentError};
use crate::channel::Channel;

/// Run Stage-2 LLM semantic scan for all skills in the plugin at `source`.
///
/// Skills are scanned concurrently with up to 4 in-flight at a time
/// (`buffer_unordered(4)`). An aggregate 5-minute timeout wraps the whole
/// batch; each individual scan is already bounded by `SCAN_TIMEOUT` (30 s) in
/// `SkillSemanticScanner`. Returns `Some(err_msg)` when any skill is blocked,
/// `None` when all skills pass.
///
/// Each future carries its own `skill_name` so that the rejection message names
/// the correct skill regardless of completion order (which differs from input
/// order when futures complete out-of-order with `buffer_unordered`).
async fn semantic_scan_plugin_add(
    scanner: &zeph_skills::semantic_scanner::SkillSemanticScanner,
    source: &str,
    managed_dir: Option<std::path::PathBuf>,
    mcp_allowed: Vec<String>,
    base_shell_allowed: Vec<String>,
) -> Result<Option<String>, CommandError> {
    use futures::stream::StreamExt as _;
    use zeph_skills::semantic_scanner::ScanVerdict;

    let plugins_dir = zeph_plugins::PluginManager::default_plugins_dir();
    let mgr_dir =
        managed_dir.unwrap_or_else(|| zeph_config::defaults::default_vault_dir().join("skills"));
    let mgr =
        zeph_plugins::PluginManager::new(plugins_dir, mgr_dir, mcp_allowed, base_shell_allowed);

    let source_owned = source.to_owned();
    let scan_inputs = tokio::task::spawn_blocking(move || mgr.scan_targets(&source_owned))
        .await
        .map_err(|e| CommandError(format!("plugin scan_targets panicked: {e}")))?
        .map_err(|e| CommandError(format!("plugin add failed: {e}")))?;

    tracing::info!(
        plugin.source = %source,
        skills_count = scan_inputs.len(),
        "plugins.add: running Stage-2 semantic scan"
    );

    // Scan all skills concurrently with up to 4 in-flight. Each individual scan is
    // already bounded by SCAN_TIMEOUT (30 s); the outer 5-min cap guards the batch.
    // Each future owns its skill_name so verdicts carry the correct name regardless
    // of buffer_unordered completion order (which is not the same as input order).
    let scan_futs: Vec<_> = scan_inputs
        .iter()
        .map(|input| {
            let name = input.skill_name.clone();
            let purpose = input.declared_purpose.clone();
            let md = input.skill_md.clone();
            async move {
                let verdict = scanner.scan(&name, &purpose, &md).await;
                (name, verdict)
            }
        })
        .collect();

    let verdicts: Vec<_> = tokio::time::timeout(
        std::time::Duration::from_mins(5),
        futures::stream::iter(scan_futs)
            .buffer_unordered(4)
            .collect::<Vec<_>>(),
    )
    .await
    .map_err(|_| CommandError("plugin scan timed out after 300s".to_owned()))?;

    for (skill_name, verdict_result) in verdicts {
        let verdict = verdict_result.map_err(|e| {
            CommandError(format!(
                "plugin add failed: semantic scan error for skill {skill_name:?}: {e}"
            ))
        })?;
        match verdict {
            ScanVerdict::Allow => {
                tracing::debug!(
                    skill = %skill_name,
                    "plugins.add: skill passed semantic scan"
                );
            }
            ScanVerdict::Warn(ref reason) => {
                tracing::warn!(
                    skill = %skill_name,
                    reason = %reason,
                    "plugins.add: skill passed with warning"
                );
            }
            ScanVerdict::Block(reason) => {
                return Ok(Some(format!(
                    "plugin add failed: skill {skill_name:?} rejected by semantic scan: {reason}"
                )));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// Format the `additional_directories` allowlist for display.
pub(super) fn format_acp_dirs(cfg: &zeph_config::AcpConfig) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "ACP additional_directories allowlist:");
    if cfg.additional_directories.is_empty() {
        let _ = writeln!(out, "  (none configured)");
    } else {
        for dir in &cfg.additional_directories {
            let _ = writeln!(out, "  {dir}");
        }
    }
    out.trim_end().to_owned()
}

/// Format the `auth_methods` list for display.
pub(super) fn format_acp_auth_methods(cfg: &zeph_config::AcpConfig) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "ACP auth_methods:");
    if cfg.auth_methods.is_empty() {
        let _ = writeln!(out, "  (none configured)");
    } else {
        for method in &cfg.auth_methods {
            let _ = writeln!(out, "  {method}");
        }
    }
    out.trim_end().to_owned()
}

/// Format the ACP server status summary.
pub(super) fn format_acp_status(cfg: &zeph_config::AcpConfig, is_acp_session: bool) -> String {
    let mut out = String::new();
    let enabled = if cfg.enabled { "enabled" } else { "disabled" };
    let _ = writeln!(out, "ACP: {enabled}");
    let _ = writeln!(out, "transport:       {:?}", cfg.transport);
    let _ = writeln!(out, "agent_name:      {}", cfg.agent_name);
    let _ = writeln!(out, "agent_version:   {}", cfg.agent_version);
    let _ = writeln!(out, "max_sessions:    {}", cfg.max_sessions);
    let _ = writeln!(out, "http_bind:       {}", cfg.http_bind);
    let _ = writeln!(out, "discovery:       {}", cfg.discovery_enabled);
    let _ = writeln!(out, "message_ids:     {}", cfg.message_ids_enabled);
    let _ = writeln!(
        out,
        "this session:    {}",
        if is_acp_session {
            "ACP client"
        } else {
            "non-ACP"
        }
    );
    out.trim_end().to_owned()
}

/// Pure dispatcher — separated from `Agent` for unit testing.
pub(super) fn dispatch_acp(
    cfg: &zeph_config::AcpConfig,
    is_acp_session: bool,
    args: &str,
) -> Result<String, AgentError> {
    match args.trim() {
        "dirs" => Ok(format_acp_dirs(cfg)),
        "auth-methods" => Ok(format_acp_auth_methods(cfg)),
        "status" => Ok(format_acp_status(cfg, is_acp_session)),
        "" => Ok(
            "Usage: /acp <subcommand>\n\nSubcommands:\n  dirs          List additional_directories allowlist\n  auth-methods  List advertised auth methods\n  status        Show ACP server configuration summary"
                .to_owned(),
        ),
        other => Err(AgentError::UnknownCommand(format!(
            "Unknown /acp subcommand: {other}. Valid subcommands: dirs, auth-methods, status"
        ))),
    }
}

impl<C: Channel> Agent<C> {
    /// Dispatch `/acp [dirs|auth-methods|status]` and return a display string.
    pub(super) fn handle_acp_as_string(&mut self, args: &str) -> Result<String, AgentError> {
        dispatch_acp(
            &self.runtime.config.acp_config,
            self.services.security.is_acp_session,
            args,
        )
    }
}

impl<C: Channel + Send + 'static> IntegrationAccess for Agent<C> {
    // ----- /plugins -----

    fn handle_plugins<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let args_owned = args.to_owned();
        // Clone the fields needed by PluginManager before entering the async block.
        // spawn_blocking requires 'static, so we cannot borrow &self inside the closure.
        let managed_dir = self.services.skill.managed_dir.clone();
        let mcp_allowed = self.services.mcp.allowed_commands.clone();
        let base_shell_allowed = self.runtime.lifecycle.startup_shell_overlay.allowed.clone();
        // Same reputation config the CLI/bootstrap paths use (spec-043, #5864) — threaded
        // through so `/plugins add` gets the identical typosquat check `zeph plugin add` does.
        let reputation_cfg = self.runtime.config.plugins_reputation.clone();
        // Collect manifest paths for ephemeral plugins. Reading the actual files is
        // deferred into the async block below to avoid blocking the tokio worker thread.
        let ephemeral_manifest_paths: Vec<std::path::PathBuf> = self
            .runtime
            .ephemeral_plugins
            .iter()
            .map(|tmp| tmp.path().join("plugin.toml"))
            .collect();

        // Resolve scanner once, before the async block captures `self`.
        // Fail-closed: if semantic_scan is enabled but no provider is configured, refuse
        // to proceed rather than silently falling back to the primary provider (#4706, #4709).
        let semantic_scan_enabled = self.services.skill.semantic_scan;
        let maybe_scanner: Option<zeph_skills::semantic_scanner::SkillSemanticScanner> =
            if semantic_scan_enabled {
                let provider_name = self.services.skill.semantic_scan_provider.as_str();
                if provider_name.trim().is_empty() {
                    return Box::pin(async move {
                        Err(CommandError::new(
                            "semantic_scan is enabled but semantic_scan_provider is not set; \
                             refusing plugin add to maintain fail-closed security posture",
                        ))
                    });
                }
                let provider_known = self
                    .runtime
                    .providers
                    .provider_pool
                    .iter()
                    .any(|e| e.effective_name().eq_ignore_ascii_case(provider_name));
                if !provider_known {
                    let name = provider_name.to_owned();
                    return Box::pin(async move {
                        Err(CommandError::new(format!(
                            "semantic_scan is enabled but semantic_scan_provider '{name}' \
                             is not configured in [[llm.providers]]; \
                             refusing plugin add to maintain fail-closed security posture",
                        )))
                    });
                }
                let provider = self.resolve_background_provider(provider_name);
                Some(zeph_skills::semantic_scanner::SkillSemanticScanner::new(
                    provider,
                ))
            } else {
                None
            };

        Box::pin(async move {
            let (subcmd, source) = args_owned
                .trim()
                .split_once(' ')
                .unwrap_or((args_owned.trim(), ""));

            // Stage-2 LLM semantic scan runs before the blocking add(), fail-closed.
            if subcmd == "add"
                && !source.trim().is_empty()
                && let Some(ref scanner) = maybe_scanner
                && let Some(err) = semantic_scan_plugin_add(
                    scanner,
                    source.trim(),
                    managed_dir.clone(),
                    mcp_allowed.clone(),
                    base_shell_allowed.clone(),
                )
                .instrument(tracing::info_span!("core.agent.scan_plugin", plugin = %source.trim()))
                .await?
            {
                return Ok(err);
            }

            // Resolve ephemeral plugin names asynchronously before entering the blocking task.
            let ephemeral_names: Vec<String> = {
                use futures::future::join_all;
                let futs = ephemeral_manifest_paths.into_iter().map(|p| async move {
                    tokio::fs::read_to_string(&p)
                        .await
                        .ok()
                        .and_then(|s| toml::from_str::<zeph_plugins::PluginManifest>(&s).ok())
                        .map(|m| m.plugin.name.to_string())
                });
                join_all(futs).await.into_iter().flatten().collect()
            };

            // PluginManager performs synchronous filesystem I/O (copy, remove_dir_all,
            // read_dir). Run on a blocking thread to avoid stalling the tokio worker.
            tokio::task::spawn_blocking(move || {
                Self::run_plugin_command(
                    &args_owned,
                    managed_dir,
                    mcp_allowed,
                    base_shell_allowed,
                    ephemeral_names,
                    &reputation_cfg,
                )
            })
            .await
            .map_err(|e| CommandError(format!("plugin task panicked: {e}")))
        })
    }

    // ----- /acp -----

    fn handle_acp<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            self.handle_acp_as_string(args)
                .map_err(|e| CommandError::new(e.to_string()))
        })
    }

    // ----- /cocoon -----

    delegate_cmd!(handle_cocoon, handle_cocoon_as_string, args: &'a str => String);
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;

    fn cfg_default() -> zeph_config::AcpConfig {
        zeph_config::AcpConfig::default()
    }

    fn cfg_with_dirs(dirs: &[&str]) -> zeph_config::AcpConfig {
        let mut cfg = cfg_default();
        cfg.additional_directories = dirs
            .iter()
            .map(|p| {
                zeph_config::AdditionalDir::parse(
                    std::path::Path::new(p)
                        .canonicalize()
                        .unwrap_or_else(|_| std::path::PathBuf::from(p)),
                )
                .unwrap_or_else(|_| panic!("failed to parse {p}"))
            })
            .collect();
        cfg
    }

    #[test]
    fn dirs_empty() {
        let out = format_acp_dirs(&cfg_default());
        assert!(out.contains("(none configured)"), "got: {out}");
    }

    #[test]
    fn dirs_populated() {
        // Use a real directory so canonicalize succeeds on all platforms.
        // Compare against the canonical form to handle macOS /tmp→/private/tmp
        // and Windows \\?\ extended-length prefix transparently.
        let tmp_dir = tempfile::tempdir().expect("tempdir");
        let canonical =
            std::fs::canonicalize(tmp_dir.path()).unwrap_or_else(|_| tmp_dir.path().to_owned());
        let canonical_str = canonical.to_string_lossy();
        let cfg = cfg_with_dirs(&[canonical_str.as_ref()]);
        let out = format_acp_dirs(&cfg);
        assert!(out.contains(canonical_str.as_ref()), "got: {out}");
        assert!(!out.contains("(none configured)"), "got: {out}");
    }

    #[test]
    fn auth_methods_default() {
        let out = format_acp_auth_methods(&cfg_default());
        assert!(out.contains("agent"), "got: {out}");
        assert!(!out.contains("Agent"), "got: {out}");
    }

    #[test]
    fn auth_methods_empty() {
        let mut cfg = cfg_default();
        cfg.auth_methods.clear();
        let out = format_acp_auth_methods(&cfg);
        assert!(out.contains("(none configured)"), "got: {out}");
    }

    #[test]
    fn status_disabled() {
        let out = format_acp_status(&cfg_default(), false);
        assert!(out.contains("ACP: disabled"), "got: {out}");
        assert!(out.contains("non-ACP"), "got: {out}");
    }

    #[test]
    fn status_enabled_acp_session() {
        let mut cfg = cfg_default();
        cfg.enabled = true;
        let out = format_acp_status(&cfg, true);
        assert!(out.contains("ACP: enabled"), "got: {out}");
        assert!(out.contains("ACP client"), "got: {out}");
    }

    #[test]
    fn empty_args_returns_help() {
        let out = dispatch_acp(&cfg_default(), false, "").unwrap();
        assert!(out.contains("Usage: /acp"), "got: {out}");
        assert!(out.contains("dirs"), "got: {out}");
        assert!(out.contains("auth-methods"), "got: {out}");
        assert!(out.contains("status"), "got: {out}");
    }

    #[test]
    fn unknown_subcommand_returns_err() {
        let err = dispatch_acp(&cfg_default(), false, "bogus").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "got: {msg}");
        assert!(
            !msg.contains("\"bogus\""),
            "should not quote arg, got: {msg}"
        );
        assert!(
            msg.contains("dirs"),
            "should list valid subcommands, got: {msg}"
        );
    }

    #[test]
    fn whitespace_args_returns_help() {
        let out = dispatch_acp(&cfg_default(), false, "   ").unwrap();
        assert!(out.contains("Usage: /acp"), "got: {out}");
    }

    // R-4706/R-4709: when semantic_scan is enabled but semantic_scan_provider is empty,
    // `plugin add` must return a CommandError immediately (fail-closed). Before this fix
    // the code fell through to resolve_background_provider which silently used the primary
    // provider, bypassing the intent that an unconfigured scanner means "do not proceed".
    #[tokio::test]
    async fn plugin_add_semantic_scan_enabled_empty_provider_returns_error() {
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_semantic_scan(true, "");

        let result = agent.handle_plugins("add some-plugin").await;
        assert!(
            result.is_err(),
            "expected CommandError for missing semantic_scan_provider, got: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("semantic_scan_provider"),
            "error message must mention semantic_scan_provider, got: {msg}"
        );
    }

    // R-4706/R-4709: when semantic_scan is disabled, plugin subcommands must proceed
    // normally regardless of whether semantic_scan_provider is set.
    #[tokio::test]
    async fn plugin_list_semantic_scan_disabled_succeeds() {
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_semantic_scan(false, "");

        // "list" does not trigger scan logic; it should succeed without error.
        let result = agent.handle_plugins("list").await;
        assert!(
            result.is_ok(),
            "plugin list must succeed when semantic_scan is disabled, got: {result:?}"
        );
    }

    // R-4706/R-4709: "plugin add" with semantic_scan disabled must reach the install path
    // rather than return a scan-related error. The install itself may fail (no real plugin
    // source), but it must NOT fail with the fail-closed error message.
    #[tokio::test]
    async fn plugin_add_semantic_scan_disabled_no_scan_error() {
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_semantic_scan(false, "");

        let result = agent.handle_plugins("add some-plugin").await;
        // The call may succeed or fail for unrelated reasons (no real plugin source),
        // but must NOT fail with the fail-closed error about semantic_scan_provider.
        if let Err(ref e) = result {
            assert!(
                !e.to_string().contains("semantic_scan_provider"),
                "must not fail with scan error when semantic_scan is disabled, got: {e}"
            );
        }
    }

    // R-4705: semantic_scan_plugin_add must scan all skills concurrently and return
    // None when every scanner call returns Allow. Verifies buffer_unordered path
    // processes N inputs without sequential bottleneck.
    #[tokio::test]
    async fn semantic_scan_plugin_add_concurrent_all_allow_returns_none() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_skills::semantic_scanner::SkillSemanticScanner;

        // MockProvider returns `{"verdict":"allow","reason":"ok"}` for every call.
        let allow_json = r#"{"verdict":"allow","reason":"ok"}"#.to_owned();
        let provider = AnyProvider::Mock(MockProvider::with_responses(vec![
            allow_json.clone(),
            allow_json,
        ]));
        let scanner = SkillSemanticScanner::new(provider);

        // Build a minimal plugin layout with two skills so scan_targets returns
        // two SkillScanInput entries.
        let tmp = tempfile::tempdir().unwrap();
        let plugin_toml = r#"
[plugin]
name = "test-plugin"
version = "0.1.0"
description = "test"

[[skills]]
path = "skill-a"

[[skills]]
path = "skill-b"
"#;
        std::fs::write(tmp.path().join("plugin.toml"), plugin_toml).unwrap();
        for name in ["skill-a", "skill-b"] {
            let skill_dir = tmp.path().join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("# {name}\n\n## Purpose\nTest skill.\n"),
            )
            .unwrap();
        }

        let result =
            semantic_scan_plugin_add(&scanner, tmp.path().to_str().unwrap(), None, vec![], vec![])
                .await;

        // All skills allowed → no error message returned.
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        assert!(
            result.unwrap().is_none(),
            "expected None (all passed) but got Some(err)"
        );
    }

    // R-4705 regression: buffer_unordered yields in completion order, not input order.
    // A Block verdict on the *second* skill (index 1) must name that second skill, not the
    // first. Before the fix, the code zipped verdicts against scan_inputs by position and
    // discarded the tuple's skill_name, so the wrong skill was reported.
    #[tokio::test]
    async fn semantic_scan_plugin_add_block_names_correct_skill() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_skills::semantic_scanner::SkillSemanticScanner;

        // First call returns Allow, second returns Block — only the second skill is rejected.
        let allow_json = r#"{"verdict":"allow","reason":"ok"}"#.to_owned();
        let block_json = r#"{"verdict":"block","reason":"malicious"}"#.to_owned();
        let provider =
            AnyProvider::Mock(MockProvider::with_responses(vec![allow_json, block_json]));
        let scanner = SkillSemanticScanner::new(provider);

        let tmp = tempfile::tempdir().unwrap();
        let plugin_toml = r#"
[plugin]
name = "test-plugin-block"
version = "0.1.0"
description = "test"

[[skills]]
path = "skill-first"

[[skills]]
path = "skill-second"
"#;
        std::fs::write(tmp.path().join("plugin.toml"), plugin_toml).unwrap();
        for name in ["skill-first", "skill-second"] {
            let skill_dir = tmp.path().join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("# {name}\n\n## Purpose\nTest skill.\n"),
            )
            .unwrap();
        }

        let result =
            semantic_scan_plugin_add(&scanner, tmp.path().to_str().unwrap(), None, vec![], vec![])
                .await;

        assert!(result.is_ok(), "expected Ok(_), got: {result:?}");
        let msg = result
            .unwrap()
            .expect("expected Some(err) for blocked skill");
        assert!(
            msg.contains("skill-second"),
            "rejection must name the blocked skill 'skill-second', got: {msg}"
        );
        assert!(
            !msg.contains("skill-first"),
            "rejection must NOT name the allowed skill 'skill-first', got: {msg}"
        );
    }

    // R-4706/R-4709: unknown provider name must also fail-closed rather than silently
    // falling back to the primary provider via resolve_background_provider.
    #[tokio::test]
    async fn plugin_add_semantic_scan_unknown_provider_returns_error() {
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_semantic_scan(true, "nonexistent_provider");

        let result = agent.handle_plugins("add some-plugin").await;
        assert!(
            result.is_err(),
            "expected CommandError for unknown semantic_scan_provider, got: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("semantic_scan_provider"),
            "error message must mention semantic_scan_provider, got: {msg}"
        );
    }
}
