// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::agent_setup;
use crate::channel::build_cli_history;
#[cfg(not(feature = "tui"))]
use crate::channel::create_channel_inner;
#[cfg(feature = "tui")]
use crate::channel::{AppChannel, create_channel_with_tui};
use crate::cli::Cli;
#[cfg(feature = "scheduler")]
use crate::scheduler::bootstrap_scheduler;
use crate::tracing_init::init_tracing;
use crate::tui_bridge::forward_status_to_stderr;
#[cfg(feature = "tui")]
use crate::tui_bridge::{TuiRunParams, run_tui_agent};

use zeph_channels::AnyChannel;
use zeph_core::agent::Agent;
use zeph_core::bootstrap::resolve_config_path;
#[cfg(not(feature = "tui"))]
use zeph_core::bootstrap::warmup_provider;
use zeph_core::bootstrap::{AppBuilder, create_mcp_registry};
#[cfg(feature = "acp")]
use zeph_core::config::AcpTransport;
use zeph_llm::{ThinkingConfig, ThinkingEffort};

#[cfg(feature = "acp-http")]
use crate::acp::run_acp_http_server;
#[cfg(feature = "acp")]
use crate::acp::{print_acp_manifest, run_acp_server};
use crate::cli::Command;
use crate::commands::agents::handle_agents_command;
use crate::commands::memory::handle_memory_command;
use crate::commands::router::handle_router_command;
#[cfg(feature = "acp")]
use crate::commands::sessions::handle_sessions_command;
use crate::commands::skill::handle_skill_command;
use crate::commands::vault::handle_vault_command;
#[cfg(feature = "a2a")]
use crate::daemon::run_daemon;
#[cfg(all(feature = "tui", feature = "a2a"))]
use crate::tui_remote::run_tui_remote;
use zeph_llm::provider::LlmProvider;

use zeph_core::config::Config;

/// Warn at startup if legacy artifact paths exist but new `.zeph/`-based paths do not.
///
/// This fires only when the config is using the new defaults, so users with explicit
/// old paths in their config are not affected.
fn check_legacy_artifact_paths(config: &Config) {
    let checks: &[(&str, &str, &str)] = &[
        ("./data/zeph.db", ".zeph/data/zeph.db", "SQLite database"),
        ("./skills", ".zeph/skills", "skills directory"),
        (".local/debug", ".zeph/debug", "debug dump directory"),
    ];
    for (old_path, new_path, description) in checks {
        let config_matches_new = match *description {
            "SQLite database" => config.memory.sqlite_path == *new_path,
            "skills directory" => config.skills.paths.iter().any(|p| p.as_str() == *new_path),
            "debug dump directory" => config.debug.output_dir.to_str() == Some(new_path),
            other => unreachable!("unknown legacy path description: {other}"),
        };
        if config_matches_new
            && std::path::Path::new(old_path).exists()
            && !std::path::Path::new(new_path).exists()
        {
            tracing::warn!(
                "Legacy {description} found at '{old_path}'. \
                 Default location changed to '{new_path}'. \
                 Move your data: mv {old_path} {new_path}"
            );
        }
    }
}

/// Merge on-disk logging config with the optional CLI `--log-file` override.
///
/// Priority: CLI flag > config file > built-in defaults.
fn resolve_logging_config(
    config_logging: zeph_core::config::LoggingConfig,
    cli_log_file: Option<&str>,
) -> zeph_core::config::LoggingConfig {
    let mut logging = config_logging;
    if let Some(p) = cli_log_file {
        p.clone_into(&mut logging.file);
    }
    logging
}

#[allow(dead_code)]
fn cli_requested_any_acp_mode(cli: &Cli) -> bool {
    #[cfg(not(any(feature = "acp", feature = "acp-http")))]
    let _ = cli;

    #[cfg(feature = "acp")]
    if cli.acp {
        return true;
    }

    #[cfg(feature = "acp-http")]
    if cli.acp_http {
        return true;
    }

    false
}

#[cfg(feature = "acp")]
fn configured_acp_autostart_transport(config: &Config, cli: &Cli) -> Option<AcpTransport> {
    if !config.acp.enabled || cli_requested_any_acp_mode(cli) {
        return None;
    }

    #[cfg(feature = "tui")]
    if cli.tui {
        // TUI owns stdin/stdout — stdio ACP transport is incompatible.
        // Allow HTTP transport only when the acp-http feature is enabled;
        // otherwise Http would silently fall back to stdio (which is also incompatible).
        return match &config.acp.transport {
            #[cfg(feature = "acp-http")]
            AcpTransport::Http => Some(AcpTransport::Http),
            _ => {
                tracing::warn!(
                    "ACP autostart skipped in TUI mode: \
                     stdio and both transports are incompatible with TUI (both own stdin/stdout); \
                     set [acp] transport = \"http\" to run ACP alongside TUI"
                );
                None
            }
        };
    }

    Some(config.acp.transport.clone())
}

#[cfg(feature = "acp")]
async fn run_configured_acp_autostart(cli: &Cli, transport: AcpTransport) -> anyhow::Result<()> {
    let config_path = cli.config.clone();
    let vault_backend = cli.vault.clone();
    let vault_key = cli.vault_key.clone();
    let vault_path = cli.vault_path.clone();

    match transport {
        AcpTransport::Stdio => {
            Box::pin(run_acp_server(
                config_path.as_deref(),
                vault_backend.as_deref(),
                vault_key.as_deref(),
                vault_path.as_deref(),
            ))
            .await
        }
        #[cfg(feature = "acp-http")]
        AcpTransport::Http => {
            Box::pin(run_acp_http_server(
                config_path.as_deref(),
                vault_backend.as_deref(),
                vault_key.as_deref(),
                vault_path.as_deref(),
                None,
                None,
            ))
            .await
        }
        #[cfg(feature = "acp-http")]
        AcpTransport::Both => {
            Box::pin(tokio::task::LocalSet::new().run_until(async move {
                let mut http_task = tokio::task::spawn_local({
                    let config_path = config_path.clone();
                    let vault_backend = vault_backend.clone();
                    let vault_key = vault_key.clone();
                    let vault_path = vault_path.clone();
                    async move {
                        Box::pin(run_acp_http_server(
                            config_path.as_deref(),
                            vault_backend.as_deref(),
                            vault_key.as_deref(),
                            vault_path.as_deref(),
                            None,
                            None,
                        ))
                        .await
                    }
                });

                tokio::select! {
                    result = run_acp_server(
                        config_path.as_deref(),
                        vault_backend.as_deref(),
                        vault_key.as_deref(),
                        vault_path.as_deref(),
                    ) => {
                        http_task.abort();
                        result
                    }
                    join = &mut http_task => match join {
                        Ok(result) => result,
                        Err(err) => Err(err.into()),
                    },
                }
            }))
            .await
        }
        #[cfg(not(feature = "acp-http"))]
        AcpTransport::Http | AcpTransport::Both => {
            tracing::warn!(
                transport = ?transport,
                "ACP autostart requested via config, but this build was compiled without the `acp-http` feature; falling back to stdio"
            );
            Box::pin(run_acp_server(
                config_path.as_deref(),
                vault_backend.as_deref(),
                vault_key.as_deref(),
                vault_path.as_deref(),
            ))
            .await
        }
    }
}

#[cfg(not(feature = "acp"))]
fn warn_if_acp_enabled_but_unavailable(config: &Config) {
    if config.acp.enabled {
        tracing::warn!(
            "ACP autostart requested via [acp] enabled = true, but this build was compiled without the `acp` feature; ignoring the setting"
        );
    }
}

#[allow(clippy::too_many_lines, clippy::large_futures)]
pub(crate) async fn run(cli: Cli) -> anyhow::Result<()> {
    // Load logging config early (sync, cheap) so every code path gets file logging.
    let config_path = resolve_config_path(cli.config.as_deref());
    let base_logging = zeph_core::config::Config::load(&config_path)
        .map(|c| c.logging)
        .unwrap_or_default();
    let logging_config = resolve_logging_config(base_logging, cli.log_file.as_deref());
    #[cfg(feature = "tui")]
    let tui_mode = cli.tui;
    #[cfg(not(feature = "tui"))]
    let tui_mode = false;
    let _tracing_guard = init_tracing(&logging_config, tui_mode);

    match cli.command {
        Some(Command::Init { output }) => return crate::init::run(output),
        Some(Command::Vault { command: vault_cmd }) => {
            return handle_vault_command(
                vault_cmd,
                cli.vault_key.as_deref(),
                cli.vault_path.as_deref(),
            );
        }
        Some(Command::Skill { command: skill_cmd }) => {
            return handle_skill_command(skill_cmd, cli.config.as_deref()).await;
        }
        Some(Command::Memory { command: mem_cmd }) => {
            return handle_memory_command(mem_cmd, cli.config.as_deref()).await;
        }
        Some(Command::Router {
            command: router_cmd,
        }) => {
            return handle_router_command(router_cmd);
        }
        Some(Command::Ingest {
            path,
            chunk_size,
            chunk_overlap,
            collection,
        }) => {
            return crate::commands::ingest::handle_ingest(
                path,
                chunk_size,
                chunk_overlap,
                collection,
                cli.config.as_deref(),
            )
            .await;
        }
        #[cfg(feature = "acp")]
        Some(Command::Sessions { command: sess_cmd }) => {
            return handle_sessions_command(sess_cmd, cli.config.as_deref()).await;
        }
        Some(Command::Agents {
            command: agents_cmd,
        }) => {
            return handle_agents_command(agents_cmd, cli.config.as_deref()).await;
        }
        Some(Command::MigrateConfig {
            config: migrate_config_path,
            in_place,
            diff,
        }) => {
            let resolved =
                resolve_config_path(migrate_config_path.as_deref().or(cli.config.as_deref()));
            return crate::commands::migrate::handle_migrate_config(&resolved, in_place, diff);
        }
        None => {}
    }

    #[cfg(feature = "a2a")]
    if cli.daemon {
        return Box::pin(run_daemon(
            cli.config.as_deref(),
            cli.vault.as_deref(),
            cli.vault_key.as_deref(),
            cli.vault_path.as_deref(),
        ))
        .await;
    }

    #[cfg(feature = "acp")]
    if cli.acp_manifest {
        print_acp_manifest();
        return Ok(());
    }

    #[cfg(feature = "acp")]
    if cli.acp {
        return Box::pin(run_acp_server(
            cli.config.as_deref(),
            cli.vault.as_deref(),
            cli.vault_key.as_deref(),
            cli.vault_path.as_deref(),
        ))
        .await;
    }

    #[cfg(feature = "acp-http")]
    if cli.acp_http {
        return Box::pin(run_acp_http_server(
            cli.config.as_deref(),
            cli.vault.as_deref(),
            cli.vault_key.as_deref(),
            cli.vault_path.as_deref(),
            cli.acp_http_bind.as_deref(),
            cli.acp_auth_token,
        ))
        .await;
    }

    #[cfg(all(feature = "tui", feature = "a2a"))]
    if let Some(url) = cli.connect {
        return run_tui_remote(url, cli.config.as_deref()).await;
    }

    #[cfg(feature = "tui")]
    let tui_active = cli.tui;

    let mut app = AppBuilder::new(
        cli.config.as_deref(),
        cli.vault.as_deref(),
        cli.vault_key.as_deref(),
        cli.vault_path.as_deref(),
    )
    .await?;

    check_legacy_artifact_paths(app.config());

    #[cfg(feature = "acp")]
    if let Some(transport) = configured_acp_autostart_transport(app.config(), &cli) {
        return Box::pin(run_configured_acp_autostart(&cli, transport)).await;
    }

    #[cfg(not(feature = "acp"))]
    warn_if_acp_enabled_but_unavailable(app.config());

    #[cfg(feature = "scheduler")]
    {
        if cli.scheduler_disable {
            app.config_mut().scheduler.enabled = false;
        }
        if let Some(tick) = cli.scheduler_tick {
            app.config_mut().scheduler.tick_interval_secs = tick;
        }
    }

    if cli.graph_memory {
        app.config_mut().memory.graph.enabled = true;
    }

    if cli.scan_skills_on_load {
        app.config_mut().skills.trust.scan_on_load = true;
    }

    if cli.no_pre_execution_verify {
        app.config_mut().security.pre_execution_verify.enabled = false;
        tracing::warn!(
            "Pre-execution verifiers disabled via --no-pre-execution-verify. \
             Tool calls will not be checked for destructive or injection patterns."
        );
    }

    #[cfg(feature = "guardrail")]
    if cli.guardrail {
        app.config_mut().security.guardrail.enabled = true;
    }

    if cli.compression_guidelines {
        // Config field and builder are unconditional; only the background
        // task spawn is feature-gated (compression-guidelines feature).
        app.config_mut().memory.compression_guidelines.enabled = true;
    }

    if cli.focus {
        app.config_mut().agent.focus.enabled = true;
    }
    if cli.no_focus {
        app.config_mut().agent.focus.enabled = false;
    }
    if cli.sidequest {
        app.config_mut().memory.sidequest.enabled = true;
    }
    if cli.no_sidequest {
        app.config_mut().memory.sidequest.enabled = false;
    }
    if let Some(strategy) = cli.pruning_strategy {
        app.config_mut().memory.compression.pruning_strategy = strategy;
    }

    // M4 fix (#2022): SideQuest eviction and Subgoal pruning are mutually exclusive.
    // Both attempt to manage tool output eviction; running them together produces
    // conflicting eviction decisions and undefined registry state.
    if app
        .config()
        .memory
        .compression
        .pruning_strategy
        .is_subgoal()
        && app.config().memory.sidequest.enabled
    {
        anyhow::bail!(
            "SideQuest eviction and Subgoal pruning are mutually exclusive. \
             Disable [memory.sidequest] enabled or switch pruning_strategy to \
             reactive|task_aware|mig|task_aware_mig."
        );
    }

    if cli.server_compaction {
        for entry in &mut app.config_mut().llm.providers {
            if entry.provider_type == zeph_core::config::ProviderKind::Claude {
                entry.server_compaction = true;
            }
        }
    }

    if cli.extended_context {
        for entry in &mut app.config_mut().llm.providers {
            if entry.provider_type == zeph_core::config::ProviderKind::Claude {
                entry.enable_extended_context = true;
            }
        }
        tracing::warn!(
            "Extended context (1M tokens) enabled via --extended-context. \
             Tokens above 200K use long-context pricing."
        );
    }

    #[cfg(feature = "lsp-context")]
    if cli.lsp_context {
        app.config_mut().lsp.enabled = true;
    }

    // CLI --policy-file overrides [tools.policy.policy_file] from config.
    #[cfg(feature = "policy-enforcer")]
    if let Some(ref path) = cli.policy_file {
        app.config_mut().tools.policy.policy_file = Some(path.display().to_string());
        app.config_mut().tools.policy.enabled = true;
    }

    if let Some(ref thinking_str) = cli.thinking {
        let thinking = parse_thinking_arg(thinking_str)?;
        for entry in &mut app.config_mut().llm.providers {
            if entry.provider_type == zeph_core::config::ProviderKind::Claude {
                entry.thinking = Some(thinking.clone());
            }
        }
    }

    // Early-exit: print experiment results from SQLite without building a provider.
    #[cfg(feature = "experiments")]
    if cli.experiment_report {
        return run_experiment_report(&app).await;
    }

    // Early-exit: run a single experiment session and exit.
    #[cfg(feature = "experiments")]
    if cli.experiment_run {
        let (provider, _status_tx, _status_rx) = app.build_provider().await?;
        return run_experiment_session(app, provider).await;
    }

    let (provider, agent_status_tx, status_rx) = app.build_provider().await?;
    let embed_model = app.embedding_model();
    let budget_tokens = app.auto_budget_tokens(&provider);

    let config = app.config();
    let permission_policy = config
        .tools
        .permission_policy(config.security.autonomy_level);

    #[cfg(feature = "tui")]
    let with_tool_events = cli.tui && cfg!(feature = "tui");
    #[cfg(not(feature = "tui"))]
    let with_tool_events = false;

    let registry = app.build_registry();
    let watchers = app.build_watchers();
    let summary_provider = app.build_summary_provider();

    let warmup_provider_clone = provider.clone();
    #[cfg(feature = "tui")]
    let warmup_handle = None::<tokio::task::JoinHandle<()>>;
    #[cfg(not(feature = "tui"))]
    let warmup_handle = {
        let p = warmup_provider_clone.clone();
        Some(tokio::spawn(async move { warmup_provider(&p).await }))
    };

    #[cfg(feature = "tui")]
    let suppress_mcp_stderr = tui_active;
    #[cfg(not(feature = "tui"))]
    let suppress_mcp_stderr = false;

    let (memory_result, tool_setup) = tokio::join!(
        app.build_memory(&provider),
        agent_setup::build_tool_setup(
            config,
            permission_policy.clone(),
            with_tool_events,
            suppress_mcp_stderr,
            app.age_vault_arc(),
            Some(agent_status_tx.clone()),
        ),
    );
    let memory = std::sync::Arc::new(memory_result?);

    let registry = std::sync::Arc::new(std::sync::RwLock::new(registry));
    let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> = registry
        .read()
        .expect("registry read lock")
        .all_meta()
        .into_iter()
        .cloned()
        .collect();
    let skill_count = all_meta_owned.len();

    // Populate trust DB for all loaded skills.
    {
        let trust_cfg = config.skills.trust.clone();
        let managed_dir = zeph_core::bootstrap::managed_skills_dir();
        for meta in &all_meta_owned {
            let source_kind = if meta.skill_dir.starts_with(&managed_dir) {
                zeph_memory::sqlite::SourceKind::Hub
            } else {
                zeph_memory::sqlite::SourceKind::Local
            };
            let initial_level = if matches!(source_kind, zeph_memory::sqlite::SourceKind::Hub) {
                &trust_cfg.default_level
            } else {
                &trust_cfg.local_level
            };
            match zeph_skills::compute_skill_hash(&meta.skill_dir) {
                Ok(current_hash) => {
                    // Check if there's an existing record to handle hash mismatch.
                    let existing = memory
                        .sqlite()
                        .load_skill_trust(&meta.name)
                        .await
                        .ok()
                        .flatten();
                    let trust_level_str = if let Some(ref row) = existing {
                        if row.blake3_hash == current_hash {
                            row.trust_level.clone()
                        } else {
                            trust_cfg.hash_mismatch_level.to_string()
                        }
                    } else {
                        initial_level.to_string()
                    };
                    let source_path = meta.skill_dir.to_str();
                    if let Err(e) = memory
                        .sqlite()
                        .upsert_skill_trust(
                            &meta.name,
                            &trust_level_str,
                            source_kind,
                            None,
                            source_path,
                            &current_hash,
                        )
                        .await
                    {
                        tracing::warn!("failed to record trust for '{}': {e:#}", meta.name);
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to compute hash for '{}': {e:#}", meta.name);
                }
            }
        }
    }

    let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta_owned.iter().collect();
    let (matcher, cli_history) = tokio::join!(
        app.build_skill_matcher(&provider, &all_meta_refs, &memory),
        build_cli_history(&memory),
    );
    if matcher.is_some() {
        tracing::info!("skill matcher initialized for {skill_count} skill(s)");
    } else {
        tracing::info!("skill matcher unavailable, using all {skill_count} skill(s)");
    }

    #[cfg(feature = "tui")]
    let (channel, tui_handle) =
        create_channel_with_tui(app.config(), tui_active, cli_history).await?;
    #[cfg(not(feature = "tui"))]
    let channel = create_channel_inner(app.config(), cli_history).await?;

    #[cfg(feature = "tui")]
    let is_cli = matches!(channel, AppChannel::Standard(AnyChannel::Cli(_)));
    #[cfg(not(feature = "tui"))]
    let is_cli = matches!(channel, AnyChannel::Cli(_));
    if is_cli {
        println!("zeph v{}", env!("CARGO_PKG_VERSION"));
    }

    let conversation_id = match memory.sqlite().latest_conversation_id().await? {
        Some(id) => id,
        None => memory.sqlite().create_conversation().await?,
    };
    tracing::info!("conversation id: {conversation_id}");

    let (shutdown_tx, shutdown_rx) = AppBuilder::build_shutdown();
    let config = app.config();

    {
        let sqlite = memory.sqlite().clone();
        let retention_secs = config.tools.overflow.retention_days.saturating_mul(86_400);
        tokio::spawn(async move {
            match sqlite.cleanup_overflow(retention_secs).await {
                Ok(n) if n > 0 => tracing::info!("cleaned up {n} stale overflow entries"),
                Ok(_) => {}
                Err(e) => tracing::warn!("overflow cleanup failed: {e}"),
            }
        });
    }

    let _eviction_handle = {
        let eviction_cancel = zeph_memory::CancellationToken::new();
        let eviction_cancel_clone = eviction_cancel.clone();
        let mut shutdown_for_eviction = shutdown_rx.clone();
        tokio::spawn(async move {
            let _ = shutdown_for_eviction.changed().await;
            eviction_cancel_clone.cancel();
        });
        let sqlite_store = std::sync::Arc::new(memory.sqlite().clone());
        zeph_memory::start_eviction_loop(
            sqlite_store,
            &config.memory.eviction,
            std::sync::Arc::new(zeph_memory::EbbinghausPolicy::default()),
            eviction_cancel,
        )
    };

    let _tier_promotion_handle = {
        let tier_cancel = zeph_memory::CancellationToken::new();
        let tier_cancel_clone = tier_cancel.clone();
        let mut shutdown_for_tiers = shutdown_rx.clone();
        tokio::spawn(async move {
            let _ = shutdown_for_tiers.changed().await;
            tier_cancel_clone.cancel();
        });
        let sqlite_store = std::sync::Arc::new(memory.sqlite().clone());
        let tier_cfg = zeph_memory::TierPromotionConfig {
            enabled: config.memory.tiers.enabled,
            promotion_min_sessions: config.memory.tiers.promotion_min_sessions,
            similarity_threshold: config.memory.tiers.similarity_threshold,
            sweep_interval_secs: config.memory.tiers.sweep_interval_secs,
            sweep_batch_size: config.memory.tiers.sweep_batch_size,
        };
        zeph_memory::start_tier_promotion_loop(
            sqlite_store,
            provider.clone(),
            tier_cfg,
            tier_cancel,
        )
    };

    #[cfg(feature = "compression-guidelines")]
    let _guidelines_handle = if config.memory.compression_guidelines.enabled {
        let guidelines_cancel = zeph_memory::CancellationToken::new();
        let guidelines_cancel_clone = guidelines_cancel.clone();
        let mut shutdown_for_guidelines = shutdown_rx.clone();
        tokio::spawn(async move {
            let _ = shutdown_for_guidelines.changed().await;
            guidelines_cancel_clone.cancel();
        });
        Some(zeph_memory::start_guidelines_updater(
            std::sync::Arc::new(memory.sqlite().clone()),
            provider.clone(),
            std::sync::Arc::clone(&memory.token_counter),
            config.memory.compression_guidelines.clone(),
            guidelines_cancel,
        ))
    } else {
        None
    };

    let skill_paths = app.skill_paths();

    let memory_executor = zeph_core::memory_tools::MemoryToolExecutor::with_validator(
        std::sync::Arc::clone(&memory),
        conversation_id,
        zeph_sanitizer::memory_validation::MemoryWriteValidator::new(
            config.security.memory_validation.clone(),
        ),
    );
    let overflow_executor = zeph_core::overflow_tools::OverflowToolExecutor::new(
        std::sync::Arc::new(memory.sqlite().clone()),
    )
    .with_conversation(conversation_id.0);
    let skill_loader_executor =
        zeph_core::SkillLoaderExecutor::new(std::sync::Arc::clone(&registry));
    let base: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> =
        std::sync::Arc::new(tool_setup.executor);
    let inner_executor =
        zeph_tools::DynExecutor(std::sync::Arc::new(zeph_tools::CompositeExecutor::new(
            skill_loader_executor,
            zeph_tools::CompositeExecutor::new(
                memory_executor,
                zeph_tools::CompositeExecutor::new(
                    overflow_executor,
                    zeph_tools::DynExecutor(base),
                ),
            ),
        )));
    // HIGH-04: PolicyGate is the outermost executor; TrustGate wraps the inner chain.
    // Order: PolicyGateExecutor → TrustGateExecutor → CompositeExecutor → ...
    #[cfg(feature = "policy-enforcer")]
    let (tool_executor, mcp_ids_handle) = {
        let trust_gated =
            zeph_tools::TrustGateExecutor::new(inner_executor, permission_policy.clone());
        let handle = trust_gated.mcp_tool_ids_handle();
        let executor = if config.tools.policy.enabled {
            match zeph_tools::PolicyEnforcer::compile(&config.tools.policy) {
                Ok(enforcer) => {
                    let policy_context =
                        std::sync::Arc::new(std::sync::RwLock::new(zeph_tools::PolicyContext {
                            trust_level: zeph_tools::TrustLevel::Trusted,
                            env: std::env::vars().collect(),
                        }));
                    let gate = zeph_tools::PolicyGateExecutor::new(
                        trust_gated,
                        std::sync::Arc::new(enforcer),
                        policy_context,
                    );
                    zeph_tools::DynExecutor(std::sync::Arc::new(gate))
                }
                Err(e) => {
                    tracing::error!(
                        "failed to compile policy rules, policy enforcement disabled: {e}"
                    );
                    zeph_tools::DynExecutor(std::sync::Arc::new(trust_gated))
                }
            }
        } else {
            zeph_tools::DynExecutor(std::sync::Arc::new(trust_gated))
        };
        (executor, handle)
    };
    #[cfg(not(feature = "policy-enforcer"))]
    let (tool_executor, mcp_ids_handle) = {
        let trust_gated =
            zeph_tools::TrustGateExecutor::new(inner_executor, permission_policy.clone());
        let handle = trust_gated.mcp_tool_ids_handle();
        (
            zeph_tools::DynExecutor(std::sync::Arc::new(trust_gated)),
            handle,
        )
    };
    let mcp_tools = tool_setup.mcp_tools;
    // Register MCP tool IDs so TrustGateExecutor can block ALL MCP tools for
    // Quarantined skills — not just those matching QUARANTINE_DENIED suffixes.
    {
        let ids: std::collections::HashSet<String> = mcp_tools
            .iter()
            .map(zeph_mcp::McpTool::sanitized_id)
            .collect();
        *mcp_ids_handle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ids;
    }
    let mcp_manager = tool_setup.mcp_manager;
    let mcp_shared_tools = tool_setup.mcp_shared_tools;
    let mcp_tool_rx = tool_setup.mcp_tool_rx;
    // Clone the Arc before it is consumed by with_mcp so LSP hooks can share it.
    #[cfg(feature = "lsp-context")]
    let lsp_mcp_manager = std::sync::Arc::clone(&mcp_manager);
    #[cfg(feature = "tui")]
    let shell_executor_for_tui = tool_setup.tool_event_rx;
    #[cfg(not(feature = "tui"))]
    let _tool_event_rx = tool_setup.tool_event_rx;

    let _skill_watcher = watchers.skill_watcher;
    let reload_rx = watchers.skill_reload_rx;
    let _config_watcher = watchers.config_watcher;
    let config_reload_rx = watchers.config_reload_rx;

    let mcp_registry = create_mcp_registry(
        config,
        &provider,
        &mcp_tools,
        &embed_model,
        app.qdrant_ops(),
    )
    .await;

    let index_pool = memory.sqlite().pool().clone();
    let index_provider = provider.clone();
    let provider_has_tools = provider.supports_tool_use();
    let index_qdrant_ops = app.qdrant_ops().cloned();
    let config_path = app.config_path().to_owned();
    let cache_pool = memory.sqlite().pool().clone();

    // Clone provider for the experiment scheduler only when the feature will actually be used.
    // The check must happen before `provider` moves into Agent::new_with_registry_arc.
    #[cfg(all(feature = "scheduler", feature = "experiments"))]
    let provider_for_experiments =
        if config.experiments.enabled && config.experiments.schedule.enabled {
            Some(std::sync::Arc::new(provider.clone()))
        } else {
            None
        };

    let session_config = zeph_core::AgentSessionConfig::from_config(config, budget_tokens);

    let agent = Agent::new_with_registry_arc(
        provider.clone(),
        channel,
        registry,
        matcher,
        config.skills.max_active_skills,
        tool_executor,
    )
    .apply_session_config(session_config)
    .with_disambiguation_threshold(config.skills.disambiguation_threshold)
    .with_skill_reload(skill_paths.clone(), reload_rx)
    .with_managed_skills_dir(zeph_core::bootstrap::managed_skills_dir())
    .with_trust_config(config.skills.trust.clone())
    .with_memory(
        std::sync::Arc::clone(&memory),
        conversation_id,
        config.memory.history_limit,
        config.memory.semantic.recall_limit,
        config.memory.summarization_threshold,
    )
    .with_compression(config.memory.compression.clone())
    .with_routing(config.memory.routing.clone())
    .with_shutdown(shutdown_rx.clone())
    .with_config_reload(config_path, config_reload_rx)
    .with_logging_config(logging_config.clone())
    .with_autosave_config(
        config.memory.autosave_assistant,
        config.memory.autosave_min_length,
    )
    .with_shutdown_summary_config(
        config.memory.shutdown_summary,
        config.memory.shutdown_summary_min_messages,
        config.memory.shutdown_summary_max_messages,
        config.memory.shutdown_summary_timeout_secs,
    )
    .with_structured_summaries(config.memory.structured_summaries)
    .with_tool_call_cutoff(config.memory.tool_call_cutoff)
    .with_hybrid_search(config.skills.hybrid_search)
    .with_compression_guidelines_config(config.memory.compression_guidelines.clone())
    .with_focus_config(config.agent.focus.clone())
    .with_sidequest_config(config.memory.sidequest.clone())
    .maybe_init_tool_schema_filter(&config.agent.tool_filter, &provider)
    .await;

    // Wire tool dependency graph if enabled (#2024).
    let agent = if config.tools.dependencies.enabled && !config.tools.dependencies.rules.is_empty()
    {
        let graph = zeph_tools::ToolDependencyGraph::new(config.tools.dependencies.rules.clone());
        let always_on: std::collections::HashSet<String> =
            config.agent.tool_filter.always_on.iter().cloned().collect();
        tracing::info!(
            rules = config.tools.dependencies.rules.len(),
            "tool dependency graph initialized"
        );
        agent
            .with_tool_dependency_graph(graph, always_on)
            .with_dependency_config(config.tools.dependencies.clone())
    } else {
        agent
    };

    #[cfg(feature = "policy-enforcer")]
    let agent = if config.tools.policy.enabled {
        agent.with_policy_config(config.tools.policy.clone())
    } else {
        agent
    };

    // Load provider-specific and explicit instruction files.
    // base_dir is the process CWD at startup — the most natural project root for local tools.
    let instruction_base =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut explicit_instruction_files = config.agent.instruction_files.clone();
    if let Some(ref p) = config.llm.instruction_file {
        explicit_instruction_files.push(p.clone());
    }
    for entry in &config.llm.providers {
        if let Some(ref p) = entry.instruction_file {
            explicit_instruction_files.push(p.clone());
        }
    }
    let (instruction_reload_tx, instruction_reload_rx) = tokio::sync::mpsc::channel(1);

    // Collect all pool provider kinds for instruction file detection.
    let mut provider_kinds: Vec<zeph_core::config::ProviderKind> = config
        .llm
        .providers
        .iter()
        .map(|e| e.provider_type)
        .collect();
    if provider_kinds.is_empty() {
        provider_kinds.push(config.llm.effective_provider());
    }
    provider_kinds.sort_unstable_by_key(|k| k.as_str());
    provider_kinds.dedup_by_key(|k| k.as_str());

    let instruction_blocks = zeph_core::instructions::load_instructions(
        &instruction_base,
        &provider_kinds,
        &explicit_instruction_files,
        config.agent.instruction_auto_detect,
    );

    let instruction_reload_state = zeph_core::instructions::InstructionReloadState {
        base_dir: instruction_base.clone(),
        provider_kinds: provider_kinds.clone(),
        explicit_files: explicit_instruction_files.clone(),
        auto_detect: config.agent.instruction_auto_detect,
    };

    // Collect parent directories of candidate instruction files to watch.
    // Only include dirs within the canonical project root to avoid watching external paths.
    let canonical_base =
        std::fs::canonicalize(&instruction_base).unwrap_or_else(|_| instruction_base.clone());
    let mut watch_dirs: Vec<std::path::PathBuf> = Vec::new();
    watch_dirs.push(instruction_base.clone());
    watch_dirs.push(instruction_base.join(".zeph"));
    if config.agent.instruction_auto_detect {
        watch_dirs.push(instruction_base.join(".claude"));
        watch_dirs.push(instruction_base.join(".claude").join("rules"));
    }
    for p in &explicit_instruction_files {
        let abs = if p.is_absolute() {
            p.clone()
        } else {
            instruction_base.join(p)
        };
        // Boundary-check: only watch dirs within the project root.
        if let Some(parent) = abs.parent()
            && let Ok(canonical_parent) = std::fs::canonicalize(parent)
            && canonical_parent.starts_with(&canonical_base)
        {
            watch_dirs.push(parent.to_path_buf());
        }
    }
    watch_dirs.sort();
    watch_dirs.dedup();

    let _instruction_watcher = if watch_dirs.is_empty() {
        tracing::debug!("no instruction watch dirs, hot-reload disabled");
        let (tx2, _rx2) = tokio::sync::mpsc::channel(1);
        zeph_core::instructions::InstructionWatcher::start(&[], tx2)
            .expect("empty-path watcher always succeeds")
    } else {
        zeph_core::instructions::InstructionWatcher::start(&watch_dirs, instruction_reload_tx)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "instruction watcher failed, hot-reload disabled");
                let (tx2, _rx2) = tokio::sync::mpsc::channel(1);
                zeph_core::instructions::InstructionWatcher::start(&[], tx2)
                    .expect("empty-path watcher always succeeds")
            })
    };

    let agent = agent
        .with_instruction_blocks(instruction_blocks)
        .with_instruction_reload(instruction_reload_rx, instruction_reload_state);

    let agent = agent_setup::apply_response_cache(
        agent,
        config.llm.response_cache_enabled,
        cache_pool,
        config.llm.response_cache_ttl_secs,
        config.llm.semantic_cache_enabled,
        zeph_core::bootstrap::effective_embedding_model(config),
    );
    let agent =
        agent_setup::apply_cost_tracker(agent, config.cost.enabled, config.cost.max_daily_cents);
    let agent = agent_setup::apply_summary_provider(agent, summary_provider);
    let agent = agent_setup::apply_quarantine_provider(agent, app.build_quarantine_provider());
    #[cfg(feature = "guardrail")]
    let agent = agent_setup::apply_guardrail(agent, app.build_guardrail_provider());

    let (code_retriever, _index_watcher, index_progress_rx) = agent_setup::apply_code_indexer(
        &config.index,
        index_qdrant_ops,
        index_provider,
        index_pool,
        is_cli,
    )
    .await;
    #[cfg(not(feature = "tui"))]
    let _ = index_progress_rx;
    let agent =
        agent_setup::apply_code_retrieval(agent, &config.index, code_retriever, provider_has_tools);
    let agent = if let Some(search_executor) = agent_setup::build_search_code_executor(
        config,
        app.qdrant_ops().cloned(),
        provider.clone(),
        memory.sqlite().pool().clone(),
        Some(std::sync::Arc::clone(&mcp_manager)),
    ) {
        agent.add_tool_executor(search_executor)
    } else {
        agent
    };

    let agent = agent.with_mcp(mcp_tools, mcp_registry, Some(mcp_manager), &config.mcp);
    let agent = agent.with_mcp_shared_tools(mcp_shared_tools);
    let agent = agent.with_mcp_tool_rx(mcp_tool_rx);

    // Wire LSP context injection hooks when the feature is enabled and configured.
    #[cfg(feature = "lsp-context")]
    let agent = if config.lsp.enabled {
        let runner = zeph_core::lsp_hooks::LspHookRunner::new(lsp_mcp_manager, config.lsp.clone());
        agent.with_lsp_hooks(runner)
    } else {
        agent
    };
    let agent = agent.with_learning(config.skills.learning.clone());
    let judge_provider = app.build_judge_provider();
    let agent = if let Some(jp) = judge_provider {
        agent.with_judge_provider(jp)
    } else {
        agent
    };

    let agent = if config.tools.anomaly.enabled {
        agent.with_anomaly_detector(zeph_tools::AnomalyDetector::new(
            config.tools.anomaly.window_size,
            config.tools.anomaly.error_threshold,
            config.tools.anomaly.critical_threshold,
        ))
    } else {
        agent
    };

    // Apply TAFC config — CLI --tafc flag takes priority over config file.
    let tafc_config = {
        let mut tafc = config.tools.tafc.clone();
        if cli.tafc {
            tafc.enabled = true;
        }
        tafc
    };
    let agent = agent.with_tafc_config(tafc_config);

    let agent = agent.with_document_config(config.memory.documents.clone());
    let agent = agent.with_graph_config(config.memory.graph.clone());

    let agent = {
        let mut mgr = zeph_core::subagent::SubAgentManager::new(config.agents.max_concurrent);
        let agent_paths = match zeph_core::subagent::resolve_agent_paths(
            &cli.agents,
            config.agents.user_agents_dir.as_ref(),
            &config.agents.extra_dirs,
        ) {
            Ok(paths) => paths,
            Err(e) => {
                return Err(anyhow::anyhow!("{e}"));
            }
        };
        if let Err(e) = mgr.load_definitions_with_sources(
            &agent_paths,
            &cli.agents,
            config.agents.user_agents_dir.as_ref(),
            &config.agents.extra_dirs,
        ) {
            tracing::warn!("sub-agent definition loading failed: {e:#}");
        }
        let agent = agent.with_orchestration_config(config.orchestration.clone());
        agent
            .with_subagent_manager(mgr)
            .with_subagent_config(config.agents.clone())
    };

    #[cfg(feature = "experiments")]
    let agent = {
        let baseline = zeph_core::experiments::ConfigSnapshot::from_config(config);
        let agent = agent
            .with_experiment_config(config.experiments.clone())
            .with_experiment_baseline(baseline);
        if let Some(ep) = app.build_eval_provider() {
            agent.with_eval_provider(ep)
        } else {
            agent
        }
    };

    #[cfg(all(feature = "scheduler", feature = "tui"))]
    let mut sched_store_for_tui: Option<std::sync::Arc<zeph_scheduler::JobStore>> = None;
    #[cfg(all(feature = "scheduler", feature = "tui"))]
    let mut sched_refresh_rx: Option<tokio::sync::watch::Receiver<()>> = None;

    #[cfg(feature = "scheduler")]
    let agent = {
        #[cfg(all(feature = "scheduler", feature = "experiments"))]
        let exp_deps = provider_for_experiments.map(|p| (p, Some(std::sync::Arc::clone(&memory))));
        #[cfg(not(all(feature = "scheduler", feature = "experiments")))]
        let exp_deps: Option<(
            std::sync::Arc<zeph_llm::any::AnyProvider>,
            Option<std::sync::Arc<zeph_memory::semantic::SemanticMemory>>,
        )> = None;
        let (agent, sched_executor) = Box::pin(bootstrap_scheduler(
            agent,
            config,
            shutdown_rx.clone(),
            exp_deps,
        ))
        .await;
        if let Some(sched_exec) = sched_executor {
            #[cfg(feature = "tui")]
            {
                sched_store_for_tui = Some(sched_exec.store());
                let (refresh_tx, refresh_rx) = tokio::sync::watch::channel(());
                sched_refresh_rx = Some(refresh_rx);
                let sched_exec = sched_exec.with_refresh_tx(refresh_tx);
                agent.add_tool_executor(sched_exec)
            }
            #[cfg(not(feature = "tui"))]
            agent.add_tool_executor(sched_exec)
        } else {
            agent
        }
    };

    // Wire debug dump: CLI flag takes priority over [debug] config section.
    // --dump-format CLI override takes priority over config.debug.format.
    let effective_format = cli.dump_format.unwrap_or(config.debug.format);
    let agent = {
        let dump_dir = cli
            .debug_dump
            .as_ref()
            .map(|p| {
                if p.as_os_str().is_empty() {
                    config.debug.output_dir.clone()
                } else {
                    p.clone()
                }
            })
            .or_else(|| {
                config
                    .debug
                    .enabled
                    .then(|| config.debug.output_dir.clone())
            });
        if let Some(ref dir) = dump_dir {
            let (agent, session_dir) =
                match zeph_core::debug_dump::DebugDumper::new(dir.as_path(), effective_format) {
                    Ok(dumper) => {
                        let session_dir = dumper.dir().to_owned();
                        (agent.with_debug_dumper(dumper), session_dir)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "debug dump initialization failed");
                        (agent, dir.clone())
                    }
                };
            // Store trace config so runtime `/dump-format trace` can create a collector (CR-04).
            let agent = agent.with_trace_config(
                dir.clone(),
                config.debug.traces.service_name.clone(),
                config.debug.traces.redact,
            );
            // When format=Trace, also wire a TracingCollector (C-03: independent of legacy dumper).
            if effective_format == zeph_core::debug_dump::DumpFormat::Trace {
                // OTLP channel is None here; wired in tracing_init.rs when otel feature enabled.
                match zeph_core::debug_dump::trace::TracingCollector::new(
                    &session_dir,
                    &config.debug.traces.service_name,
                    config.debug.traces.redact,
                    None,
                ) {
                    Ok(collector) => agent.with_trace_collector(collector),
                    Err(e) => {
                        tracing::warn!(error = %e, "trace collector initialization failed");
                        agent
                    }
                }
            } else {
                agent
            }
        } else {
            agent
        }
    };

    #[cfg(feature = "gateway")]
    if config.gateway.enabled {
        crate::gateway_spawn::spawn_gateway_server(config, shutdown_rx.clone());
    }

    #[cfg(feature = "candle")]
    let agent = agent_setup::apply_candle_stt(agent, config.llm.stt.as_ref());

    #[cfg(feature = "stt")]
    let agent = {
        let openai_base_url = config
            .llm
            .providers
            .iter()
            .find(|e| e.provider_type == zeph_core::config::ProviderKind::OpenAi)
            .map_or("https://api.openai.com/v1".to_owned(), |e| {
                e.base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.openai.com/v1".to_owned())
            });
        let api_key = config
            .secrets
            .openai_api_key
            .as_ref()
            .map_or(String::new(), |k| k.expose().to_string());
        agent_setup::apply_whisper_stt(agent, config.llm.stt.as_ref(), &openai_base_url, api_key)
    };

    let (metrics_tx, metrics_rx) =
        tokio::sync::watch::channel(zeph_core::metrics::MetricsSnapshot::default());
    // Determine active channel name for metrics.
    #[cfg(feature = "tui")]
    let active_channel_name = if tui_active {
        "tui"
    } else if config
        .telegram
        .as_ref()
        .and_then(|t| t.token.as_ref())
        .is_some()
    {
        "telegram"
    } else {
        "cli"
    };
    #[cfg(not(feature = "tui"))]
    let active_channel_name = if config
        .telegram
        .as_ref()
        .and_then(|t| t.token.as_ref())
        .is_some()
    {
        "telegram"
    } else {
        "cli"
    };

    metrics_tx.send_modify(|m| {
        config.llm.effective_model().clone_into(&mut m.model_name);
        embed_model.clone_into(&mut m.embedding_model);
        m.token_budget = u32::try_from(budget_tokens).ok();
        m.compaction_threshold = u32::try_from(budget_tokens).ok().map(|b| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let threshold =
                (f64::from(b) * f64::from(config.memory.soft_compaction_threshold)) as u32;
            threshold
        });
        config.vault.backend.clone_into(&mut m.vault_backend);
        active_channel_name.clone_into(&mut m.active_channel);
        m.self_learning_enabled = config.skills.learning.enabled;
        m.cache_enabled = config.llm.semantic_cache_enabled;
        m.autosave_enabled = config.memory.autosave_assistant;
    });
    #[cfg(all(feature = "tui", feature = "scheduler"))]
    let metrics_tx_for_sched = metrics_tx.clone();
    let extended_context = config
        .llm
        .providers
        .iter()
        .any(|e| e.enable_extended_context);
    let provider_config_snapshot = zeph_core::ProviderConfigSnapshot {
        claude_api_key: config
            .secrets
            .claude_api_key
            .as_ref()
            .map(|s| s.expose().to_owned()),
        openai_api_key: config
            .secrets
            .openai_api_key
            .as_ref()
            .map(|s| s.expose().to_owned()),
        gemini_api_key: config
            .secrets
            .gemini_api_key
            .as_ref()
            .map(|s| s.expose().to_owned()),
        compatible_api_keys: config
            .secrets
            .compatible_api_keys
            .iter()
            .map(|(k, v)| (k.clone(), v.expose().to_owned()))
            .collect(),
        llm_request_timeout_secs: config.timeouts.llm_request_timeout_secs,
        embedding_model: config.llm.embedding_model.clone(),
    };
    let agent = agent
        .with_extended_context(extended_context)
        .with_metrics(metrics_tx)
        .with_status_tx(agent_status_tx)
        .with_provider_pool(config.llm.providers.clone(), provider_config_snapshot);
    #[cfg(not(feature = "tui"))]
    drop(metrics_rx);

    #[cfg(feature = "tui")]
    let tui_metrics_rx;
    #[cfg(feature = "tui")]
    if tui_active {
        tui_metrics_rx = Some(metrics_rx);

        #[cfg(feature = "scheduler")]
        if let Some(store) = sched_store_for_tui.take() {
            let tx_clone = metrics_tx_for_sched;
            let mut shutdown = shutdown_rx.clone();
            let mut refresh_rx = sched_refresh_rx.take();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                let do_refresh = |store: &std::sync::Arc<zeph_scheduler::JobStore>,
                                  tx: &tokio::sync::watch::Sender<
                    zeph_core::metrics::MetricsSnapshot,
                >| {
                    let store = std::sync::Arc::clone(store);
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if let Ok(jobs) = store.list_jobs().await {
                            tx.send_modify(|m| {
                                m.scheduled_tasks = jobs
                                    .into_iter()
                                    .map(|(name, kind, mode, next_run)| {
                                        [name, kind, mode, next_run]
                                    })
                                    .collect();
                            });
                        }
                    });
                };
                loop {
                    tokio::select! {
                        _ = interval.tick() => do_refresh(&store, &tx_clone),
                        () = async {
                            if let Some(ref mut rx) = refresh_rx {
                                let _ = rx.changed().await;
                            } else {
                                std::future::pending::<()>().await;
                            }
                        } => do_refresh(&store, &tx_clone),
                        _ = shutdown.changed() => break,
                    }
                }
            });
        }
    } else {
        tui_metrics_rx = None;
        drop(metrics_rx);
    };

    let mut agent = agent;
    agent
        .check_vector_store_health(config.memory.vector_backend.as_str())
        .await;
    agent.sync_graph_counts().await;

    agent_setup::spawn_ctrl_c_handler(agent.cancel_signal(), shutdown_tx);
    agent.load_history().await?;

    #[cfg(feature = "tui")]
    if let Some(tui_handle) = tui_handle {
        return Box::pin(run_tui_agent(
            agent,
            TuiRunParams {
                tui_handle,
                config,
                status_rx,
                tool_rx: shell_executor_for_tui,
                metrics_rx: tui_metrics_rx,
                warmup_provider: warmup_provider_clone,
                index_progress_rx,
                cli_tafc: cli.tafc,
            },
        ))
        .await;
    }

    if let Some(handle) = warmup_handle {
        let _ = handle.await;
    }
    tokio::spawn(forward_status_to_stderr(status_rx));
    let result = Box::pin(agent.run()).await;
    agent.shutdown().await;
    Ok(result?)
}

/// Print experiment results from `SQLite` and exit. Does not require an LLM provider.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the query fails.
#[cfg(feature = "experiments")]
async fn run_experiment_report(app: &zeph_core::bootstrap::AppBuilder) -> anyhow::Result<()> {
    use zeph_memory::sqlite::SqliteStore;

    let sqlite_path = app.config().memory.sqlite_path.clone();
    let store = SqliteStore::new(&sqlite_path).await?;
    let rows = store.list_experiment_results(None, 50).await?;

    if rows.is_empty() {
        println!("No experiment results found.");
        return Ok(());
    }

    println!(
        "{:<8} {:<12} {:<20} {:<8} {:<8} {:<8} {:<8}",
        "ID", "Session", "Parameter", "Delta", "Baseline", "Candidate", "Accepted"
    );
    for r in &rows {
        let sid_len = r.session_id.len().min(11);
        // lgtm[rust/cleartext-logging]
        println!(
            "{:<8} {:<12} {:<20} {:<8.3} {:<8.3} {:<8.3} {:<8}",
            r.id,
            &r.session_id[..sid_len],
            &r.parameter,
            r.delta,
            r.baseline_score,
            r.candidate_score,
            if r.accepted { "yes" } else { "no" },
        );
    }
    Ok(())
}

/// Run a single experiment session and exit.
///
/// # Errors
///
/// Returns an error if config is invalid, benchmark fails to load, or engine fails.
#[cfg(feature = "experiments")]
async fn run_experiment_session(
    app: zeph_core::bootstrap::AppBuilder,
    provider: zeph_llm::any::AnyProvider,
) -> anyhow::Result<()> {
    use std::sync::Arc;

    use zeph_core::experiments::{
        BenchmarkSet, ConfigSnapshot, Evaluator, ExperimentEngine, ExperimentSource, GridStep,
        SearchSpace,
    };

    let config = app.config();

    if !config.experiments.enabled {
        anyhow::bail!("--experiment-run requires [experiments] enabled = true in config");
    }

    config
        .experiments
        .validate()
        .map_err(|e| anyhow::anyhow!("experiment config validation failed: {e}"))?;

    let benchmark_path =
        config.experiments.benchmark_file.clone().ok_or_else(|| {
            anyhow::anyhow!("--experiment-run requires experiments.benchmark_file")
        })?;

    let benchmark = BenchmarkSet::from_file(&benchmark_path)
        .map_err(|e| anyhow::anyhow!("failed to load benchmark: {e}"))?;

    let provider_arc = Arc::new(provider);
    // Use a dedicated eval provider when `eval_model` is configured to avoid self-judge bias.
    let judge_arc = app
        .build_eval_provider()
        .map_or_else(|| Arc::clone(&provider_arc), Arc::new);
    let evaluator = Evaluator::new(judge_arc, benchmark, config.experiments.eval_budget_tokens)
        .map_err(|e| anyhow::anyhow!("failed to create evaluator: {e}"))?;

    let generator = Box::new(GridStep::new(SearchSpace::default()));
    let baseline = ConfigSnapshot::from_config(config);
    let exp_config = config.experiments.clone();

    // Build memory for persisting results (best effort — if unavailable, results are logged only).
    let memory = app.build_memory(&provider_arc).await.ok().map(Arc::new);

    let mut engine = ExperimentEngine::new(
        evaluator,
        generator,
        provider_arc,
        baseline,
        exp_config,
        memory,
    )
    .with_source(ExperimentSource::Manual);

    // Wire Ctrl+C to cancel the engine gracefully.
    let token = engine.cancel_token();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        token.cancel();
    });

    println!("Starting experiment session...");
    let report = engine.run().await?;

    let accepted = report.results.iter().filter(|r| r.accepted).count();
    println!("\nSession:     {}", report.session_id); // lgtm[rust/cleartext-logging]
    println!(
        "Experiments: {} ({} accepted)",
        report.results.len(),
        accepted
    );
    println!("Baseline score: {:.3}", report.baseline_score);
    println!("Final score:    {:.3}", report.final_score);
    println!("Improvement:    {:.3}", report.total_improvement);
    println!("Wall time:      {} ms", report.wall_time_ms);
    if report.cancelled {
        println!("(cancelled by user)");
    }
    Ok(())
}

/// - `extended:<budget_tokens>` — e.g. `extended:10000`
/// - `adaptive` — adaptive mode with default effort
/// - `adaptive:<effort>` — effort is `low`, `medium`, or `high`
fn parse_thinking_arg(s: &str) -> anyhow::Result<ThinkingConfig> {
    const MIN_BUDGET: u32 = 1_024;
    const MAX_BUDGET: u32 = 128_000;
    if let Some(budget_str) = s.strip_prefix("extended:") {
        let budget_tokens: u32 = budget_str.parse().map_err(|_| {
            anyhow::anyhow!(
                "--thinking extended:<budget> requires a numeric token budget, got: {budget_str}"
            )
        })?;
        if !(MIN_BUDGET..=MAX_BUDGET).contains(&budget_tokens) {
            anyhow::bail!(
                "--thinking extended:{budget_tokens}: budget_tokens must be in [{MIN_BUDGET}, {MAX_BUDGET}]"
            );
        }
        return Ok(ThinkingConfig::Extended { budget_tokens });
    }
    if s == "adaptive" {
        return Ok(ThinkingConfig::Adaptive { effort: None });
    }
    if let Some(effort_str) = s.strip_prefix("adaptive:") {
        let effort = match effort_str {
            "low" => ThinkingEffort::Low,
            "medium" => ThinkingEffort::Medium,
            "high" => ThinkingEffort::High,
            other => {
                anyhow::bail!("--thinking adaptive:<effort> requires low/medium/high, got: {other}")
            }
        };
        return Ok(ThinkingConfig::Adaptive {
            effort: Some(effort),
        });
    }
    anyhow::bail!(
        "invalid --thinking value: \"{s}\". Use \"extended:<budget>\", \"adaptive\", or \"adaptive:<effort>\""
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    // --- resolve_logging_config ---

    #[test]
    fn resolve_logging_config_no_cli_no_config_file_uses_default() {
        let base = zeph_core::config::LoggingConfig::default();
        let result = resolve_logging_config(base.clone(), None);
        assert_eq!(result.file, base.file);
    }

    #[test]
    fn resolve_logging_config_no_cli_with_config_file_uses_config() {
        let base = zeph_core::config::LoggingConfig {
            file: "/var/log/zeph.log".into(),
            ..zeph_core::config::LoggingConfig::default()
        };
        let result = resolve_logging_config(base, None);
        assert_eq!(result.file, "/var/log/zeph.log");
    }

    #[test]
    fn resolve_logging_config_cli_empty_str_disables_logging() {
        let base = zeph_core::config::LoggingConfig {
            file: "/var/log/zeph.log".into(),
            ..zeph_core::config::LoggingConfig::default()
        };
        let result = resolve_logging_config(base, Some(""));
        assert_eq!(result.file, "");
    }

    #[test]
    fn resolve_logging_config_cli_path_overrides_config() {
        let base = zeph_core::config::LoggingConfig {
            file: "/var/log/zeph.log".into(),
            ..zeph_core::config::LoggingConfig::default()
        };
        let result = resolve_logging_config(base, Some("/tmp/custom.log"));
        assert_eq!(result.file, "/tmp/custom.log");
    }

    // --- parse_thinking ---

    #[test]
    fn parse_thinking_extended() {
        let cfg = parse_thinking_arg("extended:10000").unwrap();
        assert_eq!(
            cfg,
            ThinkingConfig::Extended {
                budget_tokens: 10_000
            }
        );
    }

    #[test]
    fn parse_thinking_adaptive_no_effort() {
        let cfg = parse_thinking_arg("adaptive").unwrap();
        assert_eq!(cfg, ThinkingConfig::Adaptive { effort: None });
    }

    #[test]
    fn parse_thinking_adaptive_with_effort() {
        let cfg = parse_thinking_arg("adaptive:high").unwrap();
        assert_eq!(
            cfg,
            ThinkingConfig::Adaptive {
                effort: Some(ThinkingEffort::High)
            }
        );
    }

    #[test]
    fn parse_thinking_invalid_returns_error() {
        assert!(parse_thinking_arg("unknown").is_err());
        assert!(parse_thinking_arg("extended:notanumber").is_err());
        assert!(parse_thinking_arg("adaptive:invalid").is_err());
    }

    #[test]
    fn parse_thinking_extended_budget_below_minimum_is_error() {
        assert!(parse_thinking_arg("extended:0").is_err());
        assert!(parse_thinking_arg("extended:1023").is_err());
    }

    #[test]
    fn parse_thinking_extended_budget_above_maximum_is_error() {
        assert!(parse_thinking_arg("extended:128001").is_err());
    }

    #[test]
    fn parse_thinking_extended_boundary_values_succeed() {
        assert!(parse_thinking_arg("extended:1024").is_ok());
        assert!(parse_thinking_arg("extended:128000").is_ok());
    }

    #[test]
    fn parse_thinking_adaptive_medium_effort() {
        let cfg = parse_thinking_arg("adaptive:medium").unwrap();
        assert_eq!(
            cfg,
            ThinkingConfig::Adaptive {
                effort: Some(ThinkingEffort::Medium)
            }
        );
    }

    #[test]
    fn cli_requested_any_acp_mode_is_false_without_flags() {
        let cli = Cli::parse_from(["zeph"]);
        assert!(!cli_requested_any_acp_mode(&cli));
    }

    #[cfg(feature = "acp")]
    #[test]
    fn cli_requested_any_acp_mode_is_true_for_acp_flag() {
        let cli = Cli::parse_from(["zeph", "--acp"]);
        assert!(cli_requested_any_acp_mode(&cli));
    }

    #[cfg(feature = "acp-http")]
    #[test]
    fn cli_requested_any_acp_mode_is_true_for_acp_http_flag() {
        let cli = Cli::parse_from(["zeph", "--acp-http"]);
        assert!(cli_requested_any_acp_mode(&cli));
    }

    #[cfg(feature = "acp")]
    #[test]
    fn configured_acp_autostart_transport_when_enabled_and_no_cli_override() {
        let cli = Cli::parse_from(["zeph"]);
        let mut config = Config::default();
        config.acp.enabled = true;
        assert!(matches!(
            configured_acp_autostart_transport(&config, &cli),
            Some(AcpTransport::Stdio)
        ));
    }

    #[cfg(feature = "acp")]
    #[test]
    fn configured_acp_autostart_transport_is_disabled_when_config_is_false() {
        let cli = Cli::parse_from(["zeph"]);
        let config = Config::default();
        assert!(configured_acp_autostart_transport(&config, &cli).is_none());
    }

    #[cfg(feature = "acp")]
    #[test]
    fn configured_acp_autostart_transport_is_disabled_by_acp_flag() {
        let cli = Cli::parse_from(["zeph", "--acp"]);
        let mut config = Config::default();
        config.acp.enabled = true;
        assert!(configured_acp_autostart_transport(&config, &cli).is_none());
    }

    #[cfg(feature = "acp")]
    #[test]
    fn configured_acp_autostart_transport_preserves_http_transport() {
        let cli = Cli::parse_from(["zeph"]);
        let mut config = Config::default();
        config.acp.enabled = true;
        config.acp.transport = AcpTransport::Http;
        assert!(matches!(
            configured_acp_autostart_transport(&config, &cli),
            Some(AcpTransport::Http)
        ));
    }

    #[cfg(feature = "acp")]
    #[test]
    fn configured_acp_autostart_transport_preserves_both_transport() {
        let cli = Cli::parse_from(["zeph"]);
        let mut config = Config::default();
        config.acp.enabled = true;
        config.acp.transport = AcpTransport::Both;
        assert!(matches!(
            configured_acp_autostart_transport(&config, &cli),
            Some(AcpTransport::Both)
        ));
    }

    #[cfg(all(feature = "acp", feature = "acp-http"))]
    #[test]
    fn configured_acp_autostart_transport_is_disabled_by_acp_http_flag() {
        let cli = Cli::parse_from(["zeph", "--acp-http"]);
        let mut config = Config::default();
        config.acp.enabled = true;
        assert!(configured_acp_autostart_transport(&config, &cli).is_none());
    }

    #[cfg(all(feature = "acp", feature = "tui"))]
    #[test]
    fn configured_acp_autostart_transport_suppresses_stdio_in_tui_mode() {
        let cli = Cli::parse_from(["zeph", "--tui"]);
        let mut config = Config::default();
        config.acp.enabled = true;
        config.acp.transport = AcpTransport::Stdio;
        assert!(configured_acp_autostart_transport(&config, &cli).is_none());
    }

    #[cfg(all(feature = "acp", feature = "tui"))]
    #[test]
    fn configured_acp_autostart_transport_suppresses_both_in_tui_mode() {
        let cli = Cli::parse_from(["zeph", "--tui"]);
        let mut config = Config::default();
        config.acp.enabled = true;
        config.acp.transport = AcpTransport::Both;
        assert!(configured_acp_autostart_transport(&config, &cli).is_none());
    }

    #[cfg(all(feature = "acp", feature = "tui", feature = "acp-http"))]
    #[test]
    fn configured_acp_autostart_transport_allows_http_in_tui_mode_with_acp_http() {
        let cli = Cli::parse_from(["zeph", "--tui"]);
        let mut config = Config::default();
        config.acp.enabled = true;
        config.acp.transport = AcpTransport::Http;
        assert!(matches!(
            configured_acp_autostart_transport(&config, &cli),
            Some(AcpTransport::Http)
        ));
    }

    #[cfg(all(feature = "acp", feature = "tui", not(feature = "acp-http")))]
    #[test]
    fn configured_acp_autostart_transport_suppresses_http_in_tui_mode_without_acp_http() {
        let cli = Cli::parse_from(["zeph", "--tui"]);
        let mut config = Config::default();
        config.acp.enabled = true;
        config.acp.transport = AcpTransport::Http;
        assert!(configured_acp_autostart_transport(&config, &cli).is_none());
    }
}
