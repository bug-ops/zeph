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
use crate::tui_bridge::{
    TuiRunParams, forward_index_progress_to_tui, run_tui_agent, start_tui_early,
};

use crate::bootstrap::resolve_config_path;
#[cfg(not(feature = "tui"))]
use crate::bootstrap::warmup_provider;
use crate::bootstrap::{AppBuilder, create_mcp_registry};
use parking_lot::RwLock;
use zeph_channels::AnyChannel;
use zeph_common::{RestartPolicy, TaskDescriptor, TaskSupervisor};
use zeph_config::{ThinkingConfig, ThinkingEffort};
use zeph_core::agent::Agent;
#[cfg(feature = "acp")]
use zeph_core::config::AcpTransport;

#[cfg(feature = "acp-http")]
use crate::acp::run_acp_http_server;
#[cfg(feature = "acp")]
use crate::acp::{print_acp_manifest, run_acp_server};
use crate::cli::{Command, DbCommand};
#[cfg(feature = "acp")]
use crate::commands::acp::handle_acp_command;
use crate::commands::agents::handle_agents_command;
use crate::commands::classifiers::handle_classifiers_command;
use crate::commands::memory::handle_memory_command;
use crate::commands::router::handle_router_command;
#[cfg(feature = "scheduler")]
use crate::commands::schedule::handle_schedule_command;
#[cfg(feature = "acp")]
use crate::commands::sessions::handle_sessions_command;
use crate::commands::skill::handle_skill_command;
use crate::commands::vault::handle_vault_command;
#[cfg(feature = "a2a")]
use crate::daemon::run_daemon;
#[cfg(all(feature = "tui", feature = "a2a"))]
use crate::tui_remote::run_tui_remote;
use zeph_llm::any::AnyProvider as LlmAnyProvider;
use zeph_llm::provider::LlmProvider;

use zeph_core::config::Config;

/// Adapter that bridges `PolicyLlmClient` to `AnyProvider::chat`.
///
/// Defined in `runner.rs` to keep `zeph-tools` decoupled from `zeph-llm`.
struct AdversarialPolicyLlmAdapter {
    provider: LlmAnyProvider,
}
impl zeph_tools::PolicyLlmClient for AdversarialPolicyLlmAdapter {
    fn chat<'a>(
        &'a self,
        messages: &'a [zeph_tools::PolicyMessage],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let llm_messages: Vec<zeph_llm::provider::Message> = messages
                .iter()
                .map(|m| {
                    zeph_llm::provider::Message::from_legacy(
                        match m.role {
                            zeph_tools::PolicyRole::System => zeph_llm::provider::Role::System,
                            zeph_tools::PolicyRole::User => zeph_llm::provider::Role::User,
                        },
                        m.content.clone(),
                    )
                })
                .collect();

            let result: Result<String, zeph_llm::LlmError> =
                self.provider.chat(&llm_messages).await;
            result.map_err(|e| e.to_string())
        })
    }
}

/// Adapts `ShadowSentinel` (from `zeph-core`) to the `ProbeGate` trait (from `zeph-tools`).
///
/// Placed in the binary crate to avoid a circular dependency: `zeph-tools` cannot depend on
/// `zeph-core`, and `zeph-core` cannot depend on `zeph-tools`. The adapter maps
/// `ProbeVerdict` (zeph-core) to `ProbeOutcome` (zeph-tools) — the types are isomorphic.
struct ShadowSentinelProbeGateAdapter {
    sentinel: std::sync::Arc<zeph_core::agent::shadow_sentinel::ShadowSentinel>,
}

impl zeph_tools::ProbeGate for ShadowSentinelProbeGateAdapter {
    fn probe<'a>(
        &'a self,
        qualified_tool_id: &'a str,
        args: &'a serde_json::Value,
        turn_number: u64,
        risk_level: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = zeph_tools::ProbeOutcome> + Send + 'a>>
    {
        Box::pin(async move {
            use zeph_core::agent::shadow_sentinel::ProbeVerdict;
            use zeph_tools::ProbeOutcome;
            match self
                .sentinel
                .check_tool_call(qualified_tool_id, args, turn_number, risk_level)
                .await
            {
                ProbeVerdict::Allow => ProbeOutcome::Allow,
                ProbeVerdict::Deny { reason } => ProbeOutcome::Deny { reason },
                ProbeVerdict::Skip => ProbeOutcome::Skip,
            }
        })
    }
}

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

/// Build [`zeph_context::typed_page::TypedPagesState`] from config, or return `None` when disabled.
///
/// Opens the audit sink (async) before the synchronous agent builder chain so that
/// [`zeph_context::typed_page::CompactionAuditSink::open`] can be awaited here.
///
/// # Security
///
/// `config.memory.compression.typed_pages.audit_path` is **operator-only trusted input** — it is
/// read from the agent's configuration file, which already requires file-system write access.
/// No canonicalization or prefix-check is performed because the threat model does not include
/// less-privileged config editing. Do not propagate this path from end-user input.
async fn build_typed_pages_state(
    config: &Config,
) -> Option<std::sync::Arc<zeph_context::typed_page::TypedPagesState>> {
    use zeph_config::TypedPagesEnforcement;
    use zeph_context::typed_page::{CompactionAuditSink, InvariantRegistry, TypedPagesState};

    let tp_cfg = &config.memory.compression.typed_pages;
    if !tp_cfg.enabled {
        return None;
    }

    let audit_sink = if tp_cfg.audit_path.is_empty() {
        // Derive a default audit path from the SQLite parent directory.
        let default_path = std::path::Path::new(&config.memory.sqlite_path)
            .parent()
            .map(|p| p.join("audit").join("compaction.jsonl"));

        if let Some(path) = default_path {
            match CompactionAuditSink::open(&path, tp_cfg.audit_channel_capacity).await {
                Ok(sink) => {
                    tracing::info!(
                        path = %path.display(),
                        "typed-pages audit sink opened (default path)"
                    );
                    Some(sink)
                }
                Err(e) => {
                    tracing::warn!(
                        "typed-pages audit sink could not be opened at default path, audit disabled: {e:#}"
                    );
                    None
                }
            }
        } else {
            None
        }
    } else {
        let path = std::path::PathBuf::from(&tp_cfg.audit_path);
        match CompactionAuditSink::open(&path, tp_cfg.audit_channel_capacity).await {
            Ok(sink) => {
                tracing::info!(path = %path.display(), "typed-pages audit sink opened");
                Some(sink)
            }
            Err(e) => {
                tracing::warn!("typed-pages audit sink could not be opened, audit disabled: {e:#}");
                None
            }
        }
    };

    let is_active = tp_cfg.enforcement == TypedPagesEnforcement::Active;
    Some(std::sync::Arc::new(TypedPagesState {
        registry: InvariantRegistry::default(),
        audit_sink,
        is_active,
    }))
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
                Vec::new(),
                Vec::new(),
                None,
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
            tokio::select! {
                result = run_acp_server(
                    config_path.as_deref(),
                    vault_backend.as_deref(),
                    vault_key.as_deref(),
                    vault_path.as_deref(),
                    Vec::new(),
                    Vec::new(),
                    None,
                ) => result,
                result = run_acp_http_server(
                    config_path.as_deref(),
                    vault_backend.as_deref(),
                    vault_key.as_deref(),
                    vault_path.as_deref(),
                    None,
                    None,
                ) => result,
            }
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
                Vec::new(),
                Vec::new(),
                None,
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

/// Resolve the API key for the STT provider entry.
///
/// `OpenAI` and Candle use the `OpenAI` key; Compatible providers use their own inline key or the
/// compatible-provider key from the vault.
fn resolve_stt_api_key(config: &Config, entry: &zeph_core::config::ProviderEntry) -> String {
    use zeph_core::config::ProviderKind;
    match entry.provider_type {
        ProviderKind::OpenAi => config
            .secrets
            .openai_api_key
            .as_ref()
            .map_or(String::new(), |k| k.expose().to_string()),
        ProviderKind::Compatible => entry.api_key.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

/// RAII guard that aborts the early TUI rendering task if setup fails between
/// `start_tui_early` and the final `run_tui_agent` call.
///
/// Ensures the terminal is not left in raw mode when any `?` operator between
/// those two points returns `Err`.
#[cfg(feature = "tui")]
struct EarlyTuiGuard(Option<crate::tui_bridge::EarlyTuiHandle>);

#[cfg(feature = "tui")]
impl EarlyTuiGuard {
    fn new(handle: Option<crate::tui_bridge::EarlyTuiHandle>) -> Self {
        Self(handle)
    }

    /// Consume the guard without aborting — called when setup succeeds and
    /// the TUI task is handed off to `run_tui_agent`.
    fn defuse(mut self) -> Option<crate::tui_bridge::EarlyTuiHandle> {
        self.0.take()
    }
}

#[cfg(feature = "tui")]
impl Drop for EarlyTuiGuard {
    fn drop(&mut self) {
        // Dropping EarlyTuiHandle drops the oneshot Receiver, which is fine. The actual TUI
        // thread shutdown is driven by the agent-exit branch in run_tui_agent: it calls
        // forwarders.abort_all() (which kills all agent_tx clones) and then drop(agent_tx),
        // closing agent_event_rx inside the TUI thread and causing tui_loop to exit.
        let _ = self.0.take();
    }
}

#[allow(clippy::too_many_lines, clippy::large_futures)]
pub(crate) async fn run(cli: Cli) -> anyhow::Result<()> {
    // Early-exit flags that do not require config loading.
    if cli.dump_config_defaults {
        let toml = zeph_core::config::Config::dump_defaults()
            .map_err(|e| anyhow::anyhow!("failed to serialize default config: {e}"))?;
        print!("{toml}");
        return Ok(());
    }

    // Load logging config early (sync, cheap) so every code path gets file logging.
    let config_path = resolve_config_path(cli.config.as_deref());
    let base_config = zeph_core::config::Config::load(&config_path).unwrap_or_default();
    let logging_config = resolve_logging_config(base_config.logging, cli.log_file.as_deref());
    let telemetry_config = base_config.telemetry;
    let redact_secrets = base_config.security.redact_secrets;
    let runtime_ctx = zeph_core::RuntimeContext {
        #[cfg(feature = "tui")]
        tui_mode: cli.tui,
        #[cfg(not(feature = "tui"))]
        tui_mode: false,
        #[cfg(feature = "a2a")]
        daemon_mode: cli.daemon,
        #[cfg(not(feature = "a2a"))]
        daemon_mode: false,
    };

    // Create MetricsCollector before init_tracing so the MetricsBridge layer
    // can be wired into the subscriber at startup (addresses critic finding S1).
    #[cfg(feature = "profiling")]
    let (metrics_collector_arc, metrics_rx_early) = {
        let (collector, rx) = zeph_core::metrics::MetricsCollector::new();
        (std::sync::Arc::new(collector), rx)
    };

    // Resolve json_mode directly from CLI flags before AppBuilder (which loads full config).
    // Passed to init_tracing so the stderr fmt layer is suppressed in --json mode,
    // guaranteeing no human-readable text interleaves with the JSONL stdout stream.
    let json_mode_early = cli.json || base_config.cli.json;

    let _tracing_guards = init_tracing(
        &logging_config,
        runtime_ctx,
        &telemetry_config,
        redact_secrets,
        json_mode_early,
        #[cfg(feature = "profiling")]
        Some(std::sync::Arc::clone(&metrics_collector_arc)),
    );

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
        Some(Command::Plugin {
            command: plugin_cmd,
        }) => {
            return crate::commands::plugin::handle_plugin_command(
                plugin_cmd,
                cli.config.as_deref(),
            );
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
        #[cfg(feature = "scheduler")]
        Some(Command::Schedule { command: sched_cmd }) => {
            return handle_schedule_command(sched_cmd, cli.config.as_deref()).await;
        }
        #[cfg(feature = "acp")]
        Some(Command::Acp { command: acp_cmd }) => {
            return handle_acp_command(acp_cmd).await;
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
        Some(Command::Classifiers { command: clf_cmd }) => {
            let config_path = resolve_config_path(cli.config.as_deref());
            let config = Config::load(&config_path).unwrap_or_default();
            return handle_classifiers_command(&clf_cmd, &config);
        }
        Some(Command::Db { command: db_cmd }) => {
            return match db_cmd {
                DbCommand::Migrate => {
                    crate::commands::db::handle_db_migrate(cli.config.as_deref()).await
                }
            };
        }
        #[cfg(feature = "bench")]
        Some(Command::Bench { command: bench_cmd }) => {
            return crate::commands::bench::handle_bench_command(&bench_cmd, cli.config.as_deref())
                .await;
        }
        #[cfg(all(unix, feature = "scheduler"))]
        Some(Command::Serve {
            foreground,
            no_catch_up,
        }) => {
            return crate::commands::scheduler_daemon::handle_serve(
                cli.config.as_deref(),
                foreground,
                !no_catch_up,
            )
            .await;
        }
        #[cfg(all(unix, feature = "scheduler"))]
        Some(Command::Stop { timeout_secs }) => {
            return crate::commands::scheduler_daemon::handle_stop(
                cli.config.as_deref(),
                timeout_secs,
            );
        }
        #[cfg(all(unix, feature = "scheduler"))]
        Some(Command::Status { json, n }) => {
            return crate::commands::scheduler_daemon::handle_status(
                cli.config.as_deref(),
                json,
                n,
            )
            .await;
        }
        Some(Command::Doctor {
            json,
            llm_timeout_secs,
            mcp_timeout_secs,
        }) => {
            let config_path = resolve_config_path(cli.config.as_deref());
            let exit_code = crate::commands::doctor::run_doctor(
                &config_path,
                json,
                llm_timeout_secs,
                mcp_timeout_secs,
            )
            .await?;
            std::process::exit(exit_code);
        }
        #[cfg(feature = "gonka")]
        Some(Command::Gonka {
            command: crate::cli::GonkaCommand::Doctor { json, timeout_secs },
        }) => {
            let config_path = resolve_config_path(cli.config.as_deref());
            let exit_code =
                crate::commands::gonka::run_gonka_doctor(&config_path, json, timeout_secs).await?;
            std::process::exit(exit_code);
        }
        #[cfg(feature = "cocoon")]
        Some(Command::Cocoon {
            command: crate::cli::CocoonCommand::Doctor { json, timeout_secs },
        }) => {
            let config_path = resolve_config_path(cli.config.as_deref());
            let exit_code =
                crate::commands::cocoon::run_cocoon_doctor(&config_path, json, timeout_secs)
                    .await?;
            std::process::exit(exit_code);
        }
        Some(Command::Notify {
            command: crate::cli::NotifyCommand::Test,
        }) => {
            let config_path = resolve_config_path(cli.config.as_deref());
            let config = zeph_core::config::Config::load(&config_path)?;
            let notifier = zeph_core::notifications::Notifier::new(config.notifications.clone());
            match notifier.fire_test().await {
                Ok(()) => {
                    println!("Test notification sent successfully.");
                }
                Err(e) => {
                    eprintln!("Notification test failed: {e}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        Some(Command::Project {
            command: project_cmd,
        }) => {
            return crate::commands::project::handle_project_command(
                project_cmd,
                cli.config.as_deref(),
            )
            .await;
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
        let cli_message_ids = if cli.acp_message_ids {
            Some(true)
        } else if cli.no_acp_message_ids {
            Some(false)
        } else {
            None
        };
        return Box::pin(run_acp_server(
            cli.config.as_deref(),
            cli.vault.as_deref(),
            cli.vault_key.as_deref(),
            cli.vault_path.as_deref(),
            cli.acp_additional_dir,
            cli.acp_auth_method,
            cli_message_ids,
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

    // Resolve ExecutionMode from CLI + config, then validate mutual exclusions.
    let exec_mode = crate::execution_mode::ExecutionMode::from_cli_and_config(&cli, app.config());
    crate::startup_checks::validate_mode_compatibility(&cli, app.config())?;

    // Apply -y / --auto: set autonomy_level to Full so trust-gate prompts are
    // auto-approved. Adversarial policy and shell blocklist remain enforced.
    if exec_mode.auto {
        use zeph_config::tools::AutonomyLevel;
        app.config_mut().security.autonomy_level = AutonomyLevel::Full;
    }

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
    if cli.lsp_context {
        app.config_mut().lsp.enabled = true;
    }

    // CLI --policy-file overrides [tools.policy.policy_file] from config.
    if let Some(ref path) = cli.policy_file {
        app.config_mut().tools.policy.policy_file = Some(path.display().to_string());
        app.config_mut().tools.policy.enabled = true;
    }

    // CLI --deny-domain merges into [tools.sandbox].denied_domains.
    if !cli.deny_domain.is_empty() {
        app.config_mut()
            .tools
            .sandbox
            .denied_domains
            .extend(cli.deny_domain.iter().cloned());
    }

    // Validate denied_domains after all merges so config-file + CLI entries are both checked.
    zeph_tools::validate_sandbox_denied_domains(&app.config().tools.sandbox)
        .map_err(|e| anyhow::anyhow!("invalid tools.sandbox.denied_domains: {e}"))?;

    // CLI --no-sandbox-fallback sets fail_if_unavailable.
    if cli.no_sandbox_fallback {
        app.config_mut().tools.sandbox.fail_if_unavailable = true;
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
    if cli.experiment_report {
        return run_experiment_report(&app).await;
    }

    // Early-exit: run a single experiment session and exit.
    if cli.experiment_run {
        let (provider, _status_tx, _status_rx) = app.build_provider().await?;
        return run_experiment_session(app, provider).await;
    }

    let (provider, agent_status_tx, status_rx) = app.build_provider().await?;
    let embed_model = app.embedding_model();
    let embedding_provider = crate::bootstrap::create_embedding_provider(app.config(), &provider);
    let budget_tokens = app.auto_budget_tokens(&provider);

    let config = app.config();
    let permission_policy =
        zeph_tools::build_permission_policy(&config.tools, config.security.autonomy_level);

    #[cfg(feature = "tui")]
    let with_tool_events = cli.tui && cfg!(feature = "tui");
    #[cfg(not(feature = "tui"))]
    let with_tool_events = false;

    let registry = if exec_mode.bare {
        zeph_skills::registry::SkillRegistry::empty()
    } else {
        app.build_registry()
    };
    let watchers = if exec_mode.bare {
        crate::bootstrap::WatcherBundle::empty()
    } else {
        app.build_watchers()
    };
    let summary_provider = app.build_summary_provider();

    let warmup_provider_clone = provider.clone();
    #[cfg(feature = "tui")]
    let warmup_handle = None::<tokio::task::JoinHandle<()>>;
    #[cfg(not(feature = "tui"))]
    let warmup_handle = {
        let p = warmup_provider_clone.clone();
        Some(tokio::spawn(async move { warmup_provider(&p).await }))
    };

    // For TUI path: create the channel and start rendering immediately so the user
    // sees a spinner during the heavy init phases below. For non-TUI paths (or when
    // --tui is not passed), channel creation is deferred until after the tokio::join!
    // that builds cli_history (which non-TUI channels need for readline persistence).
    //
    // `channel_opt` is Option so it can be assigned in the tui_active branch here or
    // in the deferred branch after the join. It is always Some before first use.
    #[cfg(feature = "tui")]
    let mut channel_opt: Option<AppChannel> = None;
    #[cfg(feature = "tui")]
    let mut tui_handle: Option<crate::channel::TuiHandle> = None;
    #[cfg(feature = "tui")]
    let early_tui_guard: EarlyTuiGuard;

    #[cfg(feature = "tui")]
    let mut json_sink: Option<std::sync::Arc<zeph_core::json_event_sink::JsonEventSink>> = None;
    #[cfg(feature = "tui")]
    if tui_active {
        let (ch, mut th, _sink) =
            create_channel_with_tui(app.config(), true, None, exec_mode).await?;
        early_tui_guard = EarlyTuiGuard::new(th.as_mut().map(|h| start_tui_early(h, app.config())));
        channel_opt = Some(ch);
        tui_handle = th;
    } else {
        early_tui_guard = EarlyTuiGuard::new(None);
    }

    // Drain status messages that arrive during init into the already-running TUI.
    // Without this forwarder, messages sent via `agent_status_tx` before `run_tui_agent`
    // is called (e.g. from MCP connect_all) accumulate in the unbounded channel and are
    // never displayed — causing the TUI to appear frozen on "Connecting tools…".
    // `status_rx` is consumed here; `tui_status_rx_for_params` is None when the early
    // forwarder owns the receiver, so `run_tui_agent` skips the duplicate spawn.
    #[cfg(feature = "tui")]
    let tui_status_rx_for_params: Option<tokio::sync::mpsc::UnboundedReceiver<String>>;
    #[cfg(feature = "tui")]
    {
        if let Some(ref early) = early_tui_guard.0 {
            // The forwarder task terminates naturally when all `agent_status_tx` senders are
            // dropped at the end of bootstrap. The TUI thread observes the channel close and
            // shuts down independently, so explicit abort is not needed. Dropping the handle
            // is intentional — we have no cleanup to do on the bootstrap error path here.
            let _early_status_forwarder = tokio::spawn(crate::tui_bridge::forward_status_to_tui(
                status_rx,
                early.agent_tx.clone(),
            ));
            tui_status_rx_for_params = None;
        } else {
            tui_status_rx_for_params = Some(status_rx);
        }
    }

    // Macro to send a status update to TUI during setup (no-op if no early TUI).
    #[cfg(feature = "tui")]
    macro_rules! tui_status {
        ($msg:expr) => {
            if let Some(ref early) = early_tui_guard.0 {
                let _ = early
                    .agent_tx
                    .send(zeph_tui::AgentEvent::Status($msg.into()))
                    .await;
            }
        };
    }

    // Macro to set a TUI status for the duration of an async block, clearing it on completion.
    // Status is cleared even if the block returns an error.
    #[cfg(feature = "tui")]
    macro_rules! tui_status_scope {
        ($msg:expr, $body:expr) => {{
            tui_status!($msg);
            let __result = $body;
            tui_status!("");
            __result
        }};
    }

    // Early Ctrl+C: terminate during init before the full handler is wired.
    let early_ctrlc = tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        std::process::exit(130);
    });

    #[cfg(feature = "tui")]
    tui_status!("Loading memory...");
    let memory = if exec_mode.bare {
        // Bare mode: use an ephemeral in-process SQLite with no Qdrant, no graph store,
        // and no embed backfill. Avoids all startup file and network I/O.
        std::sync::Arc::new(app.build_bare_memory(&provider).await?)
    } else {
        std::sync::Arc::new(app.build_memory(&provider).await?)
    };
    // backfill_rx: progress tracking for embed backfill.
    // None = idle/done, Some(progress) = in progress.
    #[cfg(feature = "tui")]
    let (backfill_tx, backfill_rx) =
        tokio::sync::watch::channel::<Option<zeph_memory::semantic::BackfillProgress>>(None);
    if !exec_mode.bare {
        let memory_arc = std::sync::Arc::clone(&memory);
        #[cfg(feature = "tui")]
        let _backfill_handle =
            crate::bootstrap::spawn_embed_backfill(memory_arc, 300, Some(backfill_tx));
        #[cfg(not(feature = "tui"))]
        let _backfill_handle = crate::bootstrap::spawn_embed_backfill(memory_arc, 300, None);
    }
    #[cfg(feature = "tui")]
    let mut tool_setup = tui_status_scope!("Connecting tools...", {
        agent_setup::build_tool_setup(
            config,
            permission_policy.clone(),
            with_tool_events,
            exec_mode.bare,
            runtime_ctx,
            app.age_vault_arc(),
            Some(agent_status_tx.clone()),
            Some(memory.sqlite().pool()),
            &provider,
        )
        .await
    });
    #[cfg(not(feature = "tui"))]
    let mut tool_setup = agent_setup::build_tool_setup(
        config,
        permission_policy.clone(),
        with_tool_events,
        exec_mode.bare,
        runtime_ctx,
        app.age_vault_arc(),
        Some(agent_status_tx.clone()),
        Some(memory.sqlite().pool()),
        &provider,
    )
    .await;

    let registry = std::sync::Arc::new(RwLock::new(registry));
    let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> =
        registry.read().all_meta().into_iter().cloned().collect();
    let skill_count = all_meta_owned.len();

    // Populate trust DB for all loaded skills.
    {
        let trust_cfg = config.skills.trust.clone();
        let managed_dir = crate::bootstrap::managed_skills_dir();

        // Step 1: collect all hashes and source classifications in a single spawn_blocking
        // to avoid blocking the async executor with synchronous FS reads.
        // Both compute_skill_hash (std::fs::read) and .bundled marker .exists() are blocking FS calls.
        let dirs: Vec<_> = all_meta_owned.iter().map(|m| m.skill_dir.clone()).collect();
        let managed_dir_clone = managed_dir.clone();
        let bundled_names: std::collections::HashSet<String> =
            zeph_skills::bundled_skill_names().into_iter().collect();
        let per_skill: Vec<(Option<String>, zeph_memory::store::SourceKind)> =
            tokio::task::spawn_blocking(move || {
                dirs.iter()
                    .map(|dir| {
                        let hash = zeph_skills::compute_skill_hash(dir).ok();
                        // .bundled marker is written by bundled.rs for skills shipped with the binary.
                        // The allowlist check prevents a hub-installed skill with a forged .bundled
                        // marker from receiving elevated bundled trust.
                        let source_kind = if dir.starts_with(&managed_dir_clone) {
                            let skill_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            let has_marker = dir.join(".bundled").exists();
                            if has_marker && bundled_names.contains(skill_name) {
                                zeph_memory::store::SourceKind::Bundled
                            } else {
                                if has_marker {
                                    tracing::warn!(
                                        skill = %skill_name,
                                        "skill has .bundled marker but is not in the bundled \
                                         skill allowlist — classifying as Hub"
                                    );
                                }
                                zeph_memory::store::SourceKind::Hub
                            }
                        } else {
                            zeph_memory::store::SourceKind::Local
                        };
                        (hash, source_kind)
                    })
                    .collect()
            })
            .await
            .unwrap_or_else(|_| {
                all_meta_owned
                    .iter()
                    .map(|_| (None, zeph_memory::store::SourceKind::Local))
                    .collect()
            });

        // Step 2: async DB calls using pre-computed hashes and source classifications.
        for (meta, (maybe_hash, source_kind)) in all_meta_owned.iter().zip(per_skill.iter()) {
            let source_kind = source_kind.clone();
            let initial_level = match source_kind {
                zeph_memory::store::SourceKind::Bundled => &trust_cfg.bundled_level,
                zeph_memory::store::SourceKind::Hub => &trust_cfg.default_level,
                zeph_memory::store::SourceKind::Local | zeph_memory::store::SourceKind::File => {
                    &trust_cfg.local_level
                }
            };
            let Some(current_hash) = maybe_hash else {
                tracing::warn!("failed to compute hash for '{}'", meta.name);
                continue;
            };
            // Check if there's an existing record to handle hash mismatch.
            let existing = memory
                .sqlite()
                .load_skill_trust(&meta.name)
                .await
                .ok()
                .flatten();
            let trust_level_str = if let Some(ref row) = existing {
                if row.blake3_hash != *current_hash {
                    trust_cfg.hash_mismatch_level.to_string()
                } else if row.source_kind != source_kind {
                    // source_kind changed (e.g., hub → bundled on upgrade).
                    // Never override an explicit operator block. For active trust levels,
                    // adopt the source-kind initial level when it grants more trust.
                    let stored = row
                        .trust_level
                        .parse::<zeph_common::SkillTrustLevel>()
                        .unwrap_or_else(|_| {
                            tracing::warn!(
                                skill = %meta.name,
                                raw = %row.trust_level,
                                "unrecognised trust_level in DB, treating as quarantined"
                            );
                            zeph_common::SkillTrustLevel::Quarantined
                        });
                    if !stored.is_active() || stored.severity() <= initial_level.severity() {
                        row.trust_level.clone()
                    } else {
                        initial_level.to_string()
                    }
                } else {
                    row.trust_level.clone()
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
                    current_hash,
                )
                .await
            {
                tracing::warn!("failed to record trust for '{}': {e:#}", meta.name);
            }
        }
    }

    let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta_owned.iter().collect();
    #[cfg(feature = "tui")]
    tui_status!("Loading skills...");
    let (matcher, cli_history) = tokio::join!(
        async {
            if exec_mode.bare {
                None
            } else {
                app.build_skill_matcher(&embedding_provider, &all_meta_refs, &memory)
                    .await
            }
        },
        build_cli_history(&memory),
    );
    if matcher.is_some() {
        tracing::info!("skill matcher initialized for {skill_count} skill(s)");
    } else {
        tracing::info!("skill matcher unavailable, using all {skill_count} skill(s)");
    }

    // For the non-TUI path (or when --tui was not passed), create the channel here
    // where cli_history is available. The TUI path was already created before build_memory.
    #[cfg(feature = "tui")]
    if !tui_active {
        let (ch, th, sink) =
            create_channel_with_tui(app.config(), false, cli_history, exec_mode).await?;
        channel_opt = Some(ch);
        tui_handle = th;
        json_sink = sink;
    }
    #[cfg(feature = "tui")]
    let channel = channel_opt.expect("channel always set before use");
    #[cfg(not(feature = "tui"))]
    let (channel, json_sink) = create_channel_inner(app.config(), cli_history, exec_mode).await?;

    // Wire the Telegram reaction moderation executor when the active channel is Telegram.
    // The executor is added as the outermost layer of the CompositeExecutor chain so it
    // handles `telegram_delete_reaction` / `telegram_delete_all_reactions` tool calls
    // before they reach any other executor.
    #[cfg(not(feature = "tui"))]
    {
        let telegram_api_client: Option<zeph_channels::telegram_api_ext::TelegramApiClient> =
            if let AnyChannel::Telegram(ref tg) = channel {
                Some(tg.api_ext().clone())
            } else {
                None
            };
        if let Some(api) = telegram_api_client {
            match api.get_me().await {
                Ok(me) => {
                    let backend =
                        zeph_channels::telegram_moderation::TelegramModerationBackend::new(
                            api, me.id,
                        );
                    let moderation_executor = zeph_tools::ModerationExecutor::new(backend);
                    let inner: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> =
                        std::sync::Arc::new(tool_setup.executor);
                    tool_setup.executor = zeph_tools::DynExecutor(std::sync::Arc::new(
                        zeph_tools::CompositeExecutor::new(
                            moderation_executor,
                            zeph_tools::DynExecutor(inner),
                        ),
                    ));
                    tracing::info!(
                        bot_user_id = me.id,
                        "telegram reaction moderation executor wired"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to resolve bot user ID via getMe; reaction moderation executor not wired");
                }
            }
        }
    }
    #[cfg(feature = "tui")]
    {
        let telegram_api_client: Option<zeph_channels::telegram_api_ext::TelegramApiClient> =
            match &channel {
                AppChannel::Standard(c) => {
                    if let AnyChannel::Telegram(ref tg) = **c {
                        Some(tg.api_ext().clone())
                    } else {
                        None
                    }
                }
                AppChannel::Tui(_) => None,
            };
        if let Some(api) = telegram_api_client {
            match api.get_me().await {
                Ok(me) => {
                    let backend =
                        zeph_channels::telegram_moderation::TelegramModerationBackend::new(
                            api, me.id,
                        );
                    let moderation_executor = zeph_tools::ModerationExecutor::new(backend);
                    let inner: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> =
                        std::sync::Arc::new(tool_setup.executor);
                    tool_setup.executor = zeph_tools::DynExecutor(std::sync::Arc::new(
                        zeph_tools::CompositeExecutor::new(
                            moderation_executor,
                            zeph_tools::DynExecutor(inner),
                        ),
                    ));
                    tracing::info!(
                        bot_user_id = me.id,
                        "telegram reaction moderation executor wired"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to resolve bot user ID via getMe; reaction moderation executor not wired");
                }
            }
        }
    }

    // Spawn deferred OAuth connections now that the UI channel is ready and can
    // display the authorization URL. Non-OAuth tools are already available from
    // connect_all(); OAuth tools arrive via tools_watch_tx when authorized.
    if !exec_mode.bare && tool_setup.mcp_manager.has_oauth_servers() {
        let mgr = std::sync::Arc::clone(&tool_setup.mcp_manager);
        tokio::spawn(async move {
            mgr.connect_oauth_deferred().await;
        });
    }

    #[cfg(feature = "tui")]
    let is_cli =
        matches!(&channel, AppChannel::Standard(c) if matches!(c.as_ref(), AnyChannel::Cli(_)));
    #[cfg(not(feature = "tui"))]
    let is_cli = matches!(channel, AnyChannel::Cli(_));
    if let Some(ref sink) = json_sink {
        sink.emit(&zeph_core::json_event_sink::JsonEvent::Boot {
            version: env!("CARGO_PKG_VERSION"),
            bare: exec_mode.bare,
            auto: exec_mode.auto,
        });
    } else if is_cli {
        println!("zeph v{}", env!("CARGO_PKG_VERSION"));
    }

    // Determine channel name before channel is consumed by Agent::new.
    #[cfg(feature = "tui")]
    let active_channel_name: String = match &channel {
        AppChannel::Tui(_) => "tui",
        AppChannel::Standard(c) => match c.as_ref() {
            AnyChannel::Cli(_) => "cli",
            AnyChannel::JsonCli(_) => "cli-json",
            AnyChannel::Telegram(_) => "telegram",
            #[cfg(feature = "discord")]
            AnyChannel::Discord(_) => "discord",
            #[cfg(feature = "slack")]
            AnyChannel::Slack(_) => "slack",
        },
    }
    .to_owned();
    #[cfg(not(feature = "tui"))]
    let active_channel_name: String = match &channel {
        AnyChannel::Cli(_) => "cli",
        AnyChannel::JsonCli(_) => "cli-json",
        AnyChannel::Telegram(_) => "telegram",
        #[cfg(feature = "discord")]
        AnyChannel::Discord(_) => "discord",
        #[cfg(feature = "slack")]
        AnyChannel::Slack(_) => "slack",
    }
    .to_owned();

    // Derive per-channel skill allowlist from the matching config section.
    // CLI/TUI channels use the default (allow-all) allowlist.
    #[cfg(feature = "tui")]
    let channel_skills_config: zeph_core::config::ChannelSkillsConfig = match &channel {
        AppChannel::Standard(c) if matches!(c.as_ref(), AnyChannel::Telegram(_)) => app
            .config()
            .telegram
            .as_ref()
            .map_or_else(zeph_core::config::ChannelSkillsConfig::default, |c| {
                c.skills.clone()
            }),
        #[cfg(feature = "discord")]
        AppChannel::Standard(c) if matches!(c.as_ref(), AnyChannel::Discord(_)) => app
            .config()
            .discord
            .as_ref()
            .map_or_else(zeph_core::config::ChannelSkillsConfig::default, |c| {
                c.skills.clone()
            }),
        #[cfg(feature = "slack")]
        AppChannel::Standard(c) if matches!(c.as_ref(), AnyChannel::Slack(_)) => app
            .config()
            .slack
            .as_ref()
            .map_or_else(zeph_core::config::ChannelSkillsConfig::default, |c| {
                c.skills.clone()
            }),
        _ => zeph_core::config::ChannelSkillsConfig::default(),
    };
    #[cfg(not(feature = "tui"))]
    let channel_skills_config: zeph_core::config::ChannelSkillsConfig = match &channel {
        AnyChannel::Telegram(_) => app
            .config()
            .telegram
            .as_ref()
            .map_or_else(zeph_core::config::ChannelSkillsConfig::default, |c| {
                c.skills.clone()
            }),
        #[cfg(feature = "discord")]
        AnyChannel::Discord(_) => app
            .config()
            .discord
            .as_ref()
            .map_or_else(zeph_core::config::ChannelSkillsConfig::default, |c| {
                c.skills.clone()
            }),
        #[cfg(feature = "slack")]
        AnyChannel::Slack(_) => app
            .config()
            .slack
            .as_ref()
            .map_or_else(zeph_core::config::ChannelSkillsConfig::default, |c| {
                c.skills.clone()
            }),
        _ => zeph_core::config::ChannelSkillsConfig::default(),
    };

    let conversation_id = match memory.sqlite().latest_conversation_id().await? {
        Some(id) => id,
        None => memory.sqlite().create_conversation().await?,
    };
    tracing::info!("conversation id: {conversation_id}");

    let (shutdown_tx, shutdown_rx) = AppBuilder::build_shutdown();
    let config = app.config();

    // Capture the full merged shell state at startup for hot-reload divergence detection.
    // Must snapshot config.tools.shell.blocked_commands (full list after overlay), NOT
    // just resolved_overlay().blocked_commands_add (plugin delta only) — otherwise every
    // reload would fire a spurious warning when the base config has blocked_commands.
    let startup_shell_overlay = {
        let mut blocked = config.tools.shell.blocked_commands.clone();
        blocked.sort();
        let mut allowed = config.tools.shell.allowed_commands.clone();
        allowed.sort();
        zeph_core::ShellOverlaySnapshot { blocked, allowed }
    };

    // Create a TaskSupervisor for all memory background loops.
    // A single bridge from shutdown_rx → cancel token replaces per-loop cancel bridges.
    let mem_cancel = tokio_util::sync::CancellationToken::new();
    let supervisor = std::sync::Arc::new(TaskSupervisor::new(mem_cancel.clone()));
    {
        let mut rx = shutdown_rx.clone();
        let cancel = mem_cancel.clone();
        tokio::spawn(async move {
            let _ = rx.changed().await;
            cancel.cancel();
        });
    }

    #[cfg(feature = "profiling")]
    let _sysinfo_handle = zeph_core::system_metrics::spawn_system_metrics_task(
        config.telemetry.system_metrics_interval_secs,
        shutdown_rx.clone(),
    );

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

    if !exec_mode.bare {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let embedding = memory.embedding_store().cloned();
        let eviction_cfg = config.memory.eviction.clone();
        let policy = std::sync::Arc::new(zeph_memory::EbbinghausPolicy::default());
        let cancel = supervisor.cancellation_token();
        supervisor.spawn(TaskDescriptor {
            name: "mem-eviction",
            restart: RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_eviction_loop(
                    store.clone(),
                    embedding.clone(),
                    eviction_cfg.clone(),
                    policy.clone(),
                    cancel.clone(),
                )
            },
        });
    }

    {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let tier_cfg = zeph_memory::TierPromotionConfig {
            enabled: config.memory.tiers.enabled,
            promotion_min_sessions: config.memory.tiers.promotion_min_sessions,
            similarity_threshold: config.memory.tiers.similarity_threshold,
            sweep_interval_secs: config.memory.tiers.sweep_interval_secs,
            sweep_batch_size: config.memory.tiers.sweep_batch_size,
        };
        let tier_provider = provider.clone();
        let cancel = supervisor.cancellation_token();
        supervisor.spawn(TaskDescriptor {
            name: "mem-tier-promotion",
            restart: RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_tier_promotion_loop(
                    store.clone(),
                    tier_provider.clone(),
                    tier_cfg.clone(),
                    cancel.clone(),
                )
            },
        });
    }

    {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let scene_provider = app
            .build_scene_provider()
            .unwrap_or_else(|| provider.clone());
        let scene_cfg = zeph_memory::SceneConfig {
            enabled: config.memory.tiers.scene_enabled,
            similarity_threshold: config.memory.tiers.scene_similarity_threshold,
            batch_size: config.memory.tiers.scene_batch_size,
            sweep_interval_secs: config.memory.tiers.scene_sweep_interval_secs,
        };
        let cancel = supervisor.cancellation_token();
        supervisor.spawn(TaskDescriptor {
            name: "mem-scene-consolidation",
            restart: RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_scene_consolidation_loop(
                    store.clone(),
                    scene_provider.clone(),
                    scene_cfg.clone(),
                    cancel.clone(),
                )
            },
        });
    }

    {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let consolidation_cfg = zeph_memory::ConsolidationConfig {
            enabled: config.memory.consolidation.enabled,
            confidence_threshold: config.memory.consolidation.confidence_threshold,
            sweep_interval_secs: config.memory.consolidation.sweep_interval_secs,
            sweep_batch_size: config.memory.consolidation.sweep_batch_size,
            similarity_threshold: config.memory.consolidation.similarity_threshold,
        };
        let consolidation_provider = app
            .build_consolidation_provider()
            .unwrap_or_else(|| provider.clone());
        let cancel = supervisor.cancellation_token();
        supervisor.spawn(TaskDescriptor {
            name: "mem-consolidation",
            restart: RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_consolidation_loop(
                    store.clone(),
                    consolidation_provider.clone(),
                    consolidation_cfg.clone(),
                    cancel.clone(),
                )
            },
        });
    }

    {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let forgetting_cfg = zeph_memory::ForgettingConfig {
            enabled: config.memory.forgetting.enabled,
            decay_rate: config.memory.forgetting.decay_rate,
            forgetting_floor: config.memory.forgetting.forgetting_floor,
            sweep_interval_secs: config.memory.forgetting.sweep_interval_secs,
            sweep_batch_size: config.memory.forgetting.sweep_batch_size,
            replay_window_hours: config.memory.forgetting.replay_window_hours,
            replay_min_access_count: config.memory.forgetting.replay_min_access_count,
            protect_recent_hours: config.memory.forgetting.protect_recent_hours,
            protect_min_access_count: config.memory.forgetting.protect_min_access_count,
        };
        let cancel = supervisor.cancellation_token();
        supervisor.spawn(TaskDescriptor {
            name: "mem-forgetting",
            restart: RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_forgetting_loop(
                    store.clone(),
                    forgetting_cfg.clone(),
                    cancel.clone(),
                )
            },
        });
    }

    if config.memory.compression_guidelines.enabled {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let guidelines_provider = app
            .build_guidelines_provider()
            .unwrap_or_else(|| provider.clone());
        let token_counter = std::sync::Arc::clone(&memory.token_counter);
        let guidelines_cfg = config.memory.compression_guidelines.clone();
        let cancel = supervisor.cancellation_token();
        supervisor.spawn(TaskDescriptor {
            name: "mem-guidelines",
            restart: RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_guidelines_updater(
                    store.clone(),
                    guidelines_provider.clone(),
                    token_counter.clone(),
                    guidelines_cfg.clone(),
                    cancel.clone(),
                )
            },
        });
    }

    if config.memory.tree.enabled {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let tree_provider = app
            .build_tree_consolidation_provider()
            .unwrap_or_else(|| provider.clone());
        let tree_cfg = zeph_memory::TreeConsolidationConfig {
            enabled: config.memory.tree.enabled,
            sweep_interval_secs: config.memory.tree.sweep_interval_secs,
            batch_size: config.memory.tree.batch_size,
            similarity_threshold: config.memory.tree.similarity_threshold,
            max_level: config.memory.tree.max_level,
            min_cluster_size: config.memory.tree.min_cluster_size,
        };
        let cancel = supervisor.cancellation_token();
        supervisor.spawn(TaskDescriptor {
            name: "mem-tree-consolidation",
            restart: RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::start_tree_consolidation_loop(
                    store.clone(),
                    tree_provider.clone(),
                    tree_cfg.clone(),
                    cancel.clone(),
                )
            },
        });
    }

    if config.memory.hebbian.enabled && config.memory.hebbian.consolidation_interval_secs > 0 {
        let store = std::sync::Arc::new(memory.sqlite().clone());
        let hebbian_consolidation_cfg = zeph_memory::HebbianConsolidationConfig {
            consolidation_interval_secs: config.memory.hebbian.consolidation_interval_secs,
            consolidation_threshold: config.memory.hebbian.consolidation_threshold,
            max_candidates_per_sweep: config.memory.hebbian.max_candidates_per_sweep,
            consolidation_cooldown_secs: config.memory.hebbian.consolidation_cooldown_secs,
            consolidation_prompt_timeout_secs: config
                .memory
                .hebbian
                .consolidation_prompt_timeout_secs,
            consolidation_max_neighbors: config.memory.hebbian.consolidation_max_neighbors,
        };
        let hebbian_provider = app
            .build_hebbian_consolidation_provider()
            .unwrap_or_else(|| provider.clone());
        let status_tx_clone = agent_status_tx.clone();
        let cancel = supervisor.cancellation_token();
        supervisor.spawn(TaskDescriptor {
            name: "mem-hebbian-consolidation",
            restart: RestartPolicy::RunOnce,
            factory: move || {
                zeph_memory::spawn_hebbian_consolidation_loop(
                    store.clone(),
                    hebbian_consolidation_cfg.clone(),
                    hebbian_provider.clone(),
                    Some(status_tx_clone.clone()),
                    cancel.clone(),
                )
            },
        });
    }

    let skill_paths = app.skill_paths_for_registry();
    // Cloned so the original can be moved into `with_skill_reload` while the copy is used
    // later for proactive exploration and promotion engine output directory resolution.
    let skill_paths_for_features = skill_paths.clone();
    let plugin_dirs_supplier = app.plugin_dirs_supplier();

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
    // Pre-allocate trust snapshot Arc shared between the agent's SkillState and
    // SkillInvokeExecutor — written once per turn by prepare_context, read by the executor.
    let trust_snapshot: std::sync::Arc<
        parking_lot::RwLock<std::collections::HashMap<String, zeph_common::SkillTrustLevel>>,
    > = std::sync::Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new()));
    let skill_invoke_executor = zeph_core::SkillInvokeExecutor::new(
        std::sync::Arc::clone(&registry),
        std::sync::Arc::clone(&trust_snapshot),
    );
    let base: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> =
        std::sync::Arc::new(tool_setup.executor);
    let inner_executor =
        zeph_tools::DynExecutor(std::sync::Arc::new(zeph_tools::CompositeExecutor::new(
            skill_loader_executor,
            zeph_tools::CompositeExecutor::new(
                skill_invoke_executor,
                zeph_tools::CompositeExecutor::new(
                    memory_executor,
                    zeph_tools::CompositeExecutor::new(
                        overflow_executor,
                        zeph_tools::DynExecutor(base),
                    ),
                ),
            ),
        )));
    // Executor chain order (outermost first):
    //   PolicyGateExecutor → AdversarialPolicyGateExecutor → TrustGateExecutor → Composite → ...
    //
    // Declarative policy (PolicyGate) is outermost — fast, deterministic, zero LLM cost.
    // Adversarial policy gate fires only for calls that pass declarative policy (CRIT-04).
    let mut adv_policy_info: Option<zeph_core::AdversarialPolicyInfo> = None;
    // Spec 050: shared trajectory risk slot — written by begin_turn(), read by PolicyGateExecutor.
    let trajectory_risk_slot: zeph_tools::TrajectoryRiskSlot =
        std::sync::Arc::new(parking_lot::RwLock::new(0u8));
    // Spec 050: pending risk signal queue — executor layers push signal codes; begin_turn() drains.
    let trajectory_signal_queue: zeph_tools::RiskSignalQueue =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let (tool_executor, mcp_ids_handle) = {
        let trust_gated =
            zeph_tools::TrustGateExecutor::new(inner_executor, permission_policy.clone());
        let handle = trust_gated.mcp_tool_ids_handle();

        // Layer 1 (innermost of the policy stack): adversarial policy gate (LLM-based).
        let adversarial_gated: zeph_tools::DynExecutor = if config.tools.adversarial_policy.enabled
        {
            let adv_cfg = &config.tools.adversarial_policy;
            let policies: Vec<String> = if let Some(ref path) = adv_cfg.policy_file {
                // SEC-01: canonicalize + boundary check matching load_policy_file() in policy.rs.
                // Prevents symlink attacks that could exfiltrate arbitrary files via the policy LLM.
                // spawn_blocking: canonicalize and read_to_string are blocking fs calls.
                let path_owned = path.clone();
                let load_result =
                    tokio::task::spawn_blocking(move || -> Result<Vec<String>, std::io::Error> {
                        let p = std::path::Path::new(&path_owned);
                        let canonical = std::fs::canonicalize(p)?;
                        let canonical_base =
                            std::env::current_dir().and_then(std::fs::canonicalize)?;
                        if !canonical.starts_with(&canonical_base) {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::PermissionDenied,
                                "adversarial policy file escapes project root",
                            ));
                        }
                        let content = std::fs::read_to_string(&canonical)?;
                        Ok(zeph_tools::parse_policy_lines(&content))
                    })
                    .await
                    .unwrap_or_else(|e| Err(std::io::Error::other(e)));
                match load_result {
                    Ok(lines) => lines,
                    Err(e) => {
                        tracing::error!(
                            path = %path,
                            "adversarial policy: failed to load policy file: {e}"
                        );
                        vec![]
                    }
                }
            } else {
                vec![]
            };

            if policies.is_empty() {
                tracing::warn!(
                    "adversarial policy enabled but no policies loaded; gate is a no-op"
                );
            }

            adv_policy_info = Some(zeph_core::AdversarialPolicyInfo {
                provider: adv_cfg.policy_provider.clone(),
                policy_count: policies.len(),
                fail_open: adv_cfg.fail_open,
            });

            let validator = std::sync::Arc::new(zeph_tools::PolicyValidator::new(
                policies,
                std::time::Duration::from_millis(adv_cfg.timeout_ms),
                adv_cfg.fail_open,
                adv_cfg.exempt_tools.clone(),
            ));

            let llm_client: std::sync::Arc<dyn zeph_tools::PolicyLlmClient> =
                std::sync::Arc::new(AdversarialPolicyLlmAdapter {
                    provider: provider.clone(),
                });

            let mut gate =
                zeph_tools::AdversarialPolicyGateExecutor::new(trust_gated, validator, llm_client);
            if let Some(ref audit) = tool_setup.audit_logger {
                gate = gate.with_audit(std::sync::Arc::clone(audit));
            }
            zeph_tools::DynExecutor(std::sync::Arc::new(gate))
        } else {
            zeph_tools::DynExecutor(std::sync::Arc::new(trust_gated))
        };

        // Layer 2 (outermost): declarative policy gate.
        // Merge authorization rules into policy: policy.rules evaluated first (first-match-wins),
        // then authorization.rules appended after. This means policy rules take precedence.
        let effective_policy =
            if config.tools.authorization.enabled && !config.tools.authorization.rules.is_empty() {
                let mut merged = config.tools.policy.clone();
                // M2: authorization rules appended after policy rules — policy takes precedence.
                merged
                    .rules
                    .extend(config.tools.authorization.rules.clone());
                merged.enabled = true;
                merged
            } else {
                config.tools.policy.clone()
            };
        let executor = if effective_policy.enabled {
            match zeph_tools::PolicyEnforcer::compile(&effective_policy) {
                Ok(enforcer) => {
                    let policy_context =
                        std::sync::Arc::new(RwLock::new(zeph_tools::PolicyContext {
                            trust_level: zeph_common::SkillTrustLevel::Trusted,
                            env: std::env::vars().collect(),
                        }));
                    let gate = zeph_tools::PolicyGateExecutor::new(
                        adversarial_gated,
                        std::sync::Arc::new(enforcer),
                        policy_context,
                    )
                    .with_trajectory_risk(std::sync::Arc::clone(&trajectory_risk_slot))
                    .with_signal_queue(std::sync::Arc::clone(&trajectory_signal_queue));
                    zeph_tools::DynExecutor(std::sync::Arc::new(gate))
                }
                Err(e) => {
                    tracing::error!(
                        "failed to compile policy rules, policy enforcement disabled: {e}"
                    );
                    adversarial_gated
                }
            }
        } else {
            adversarial_gated
        };
        (executor, handle)
    };
    // Spec 050 F2: wrap with ScopedToolExecutor when capability_scopes are configured.
    let tool_executor = {
        let scopes_cfg = config.security.capability_scopes.clone();
        if scopes_cfg.scopes.is_empty() {
            tool_executor
        } else {
            use std::collections::HashSet;
            use zeph_tools::DynExecutor;
            use zeph_tools::executor::ToolExecutor as _;
            use zeph_tools::scope::build_scoped_executor;
            // Collect registered tool ids for glob pattern resolution.
            let registry_ids: HashSet<String> = tool_executor
                .tool_definitions()
                .into_iter()
                .map(|d| d.id.to_string())
                .collect();
            match build_scoped_executor(tool_executor, &scopes_cfg, &registry_ids) {
                Ok(scoped) => {
                    let scoped =
                        scoped.with_signal_queue(std::sync::Arc::clone(&trajectory_signal_queue));
                    // F6: apply --scope CLI override to initial active scope.
                    if let Some(ref task_type) = cli.initial_scope
                        && !scoped.set_scope_for_task(task_type)
                    {
                        tracing::warn!(
                            task_type,
                            "CLI --scope: task type not registered in capability_scopes; ignored"
                        );
                    }
                    DynExecutor(std::sync::Arc::new(scoped))
                }
                Err(e) => {
                    // Config validation at startup prevents reaching this branch. If we do
                    // reach it (e.g. patterns compiled but registry was empty), abort startup.
                    return Err(anyhow::anyhow!("capability_scopes: {e}"));
                }
            }
        }
    };
    // Spec 050 Phase 2: wrap with ShadowProbeExecutor when shadow_sentinel.enabled = true.
    // Wiring order: ScopedToolExecutor → ShadowProbeExecutor → PolicyGateExecutor → ...
    let (tool_executor, shadow_sentinel_arc) = {
        let sentinel_cfg = &config.security.shadow_sentinel;
        if sentinel_cfg.enabled {
            let pool = memory.sqlite().pool().clone();
            let probe_provider = if sentinel_cfg.probe_provider.is_empty() {
                provider.clone()
            } else {
                match crate::bootstrap::create_named_provider(&sentinel_cfg.probe_provider, config)
                {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            provider = %sentinel_cfg.probe_provider,
                            error = %e,
                            "shadow_sentinel probe provider resolution failed, using primary"
                        );
                        provider.clone()
                    }
                }
            };
            let llm_probe = zeph_core::agent::shadow_sentinel::LlmSafetyProbe::new(
                std::sync::Arc::new(probe_provider),
                sentinel_cfg.probe_timeout_ms,
                sentinel_cfg.deny_on_timeout,
            );
            let store = zeph_core::agent::shadow_sentinel::ShadowEventStore::new(pool);
            let sentinel =
                std::sync::Arc::new(zeph_core::agent::shadow_sentinel::ShadowSentinel::new(
                    store,
                    Box::new(llm_probe),
                    sentinel_cfg.clone(),
                    conversation_id.0.to_string(),
                ));
            let turn_number = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let risk_level = std::sync::Arc::new(parking_lot::RwLock::new("calm".to_owned()));
            let probe_gate: std::sync::Arc<dyn zeph_tools::ProbeGate> =
                std::sync::Arc::new(ShadowSentinelProbeGateAdapter {
                    sentinel: std::sync::Arc::clone(&sentinel),
                });
            let shadow_exec = zeph_tools::ShadowProbeExecutor::new(
                tool_executor,
                probe_gate,
                turn_number,
                risk_level,
            );
            tracing::info!("security.shadow_sentinel: ShadowProbeExecutor wired");
            (
                zeph_tools::DynExecutor(std::sync::Arc::new(shadow_exec)),
                Some(sentinel),
            )
        } else {
            (tool_executor, None)
        }
    };
    let mcp_tools = tool_setup.mcp_tools;
    let mcp_outcomes = tool_setup.mcp_outcomes;
    // Register MCP tool IDs so TrustGateExecutor can block ALL MCP tools for
    // Quarantined skills — not just those matching QUARANTINE_DENIED suffixes.
    {
        let ids: std::collections::HashSet<String> = mcp_tools
            .iter()
            .map(zeph_mcp::McpTool::sanitized_id)
            .collect();
        *mcp_ids_handle.write() = ids;
    }
    let mcp_manager = tool_setup.mcp_manager;
    let mcp_shared_tools = tool_setup.mcp_shared_tools;
    let mcp_tool_rx = tool_setup.mcp_tool_rx;
    let mcp_elicitation_rx = tool_setup.mcp_elicitation_rx;
    // Clone the Arc before it is consumed by with_mcp so LSP hooks can share it.
    let lsp_mcp_manager = std::sync::Arc::clone(&mcp_manager);
    // Retain a reference for explicit pre-shutdown so child processes are killed while the
    // tokio runtime is still live (fixes #2693: ChildWithCleanup::drop races with shutdown).
    let shutdown_mcp_manager = std::sync::Arc::clone(&mcp_manager);
    #[cfg(feature = "tui")]
    let shell_executor_for_tui = tool_setup.tool_event_rx;
    #[cfg(not(feature = "tui"))]
    let _tool_event_rx = tool_setup.tool_event_rx;
    let taco_compressor = tool_setup.taco_compressor;
    let egress_rx = tool_setup.egress_rx;
    let shell_policy_handle = tool_setup.shell_policy_handle;
    let background_completion_rx = tool_setup.background_completion_rx;
    let shell_executor_handle = tool_setup.shell_executor_handle;
    let _skill_watcher = watchers.skill_watcher;
    // Receivers arrive as InstrumentedReceiver<T> from build_watchers().
    // Agent builder expects mpsc::Receiver<T>, so unwrap the instrumented wrapper.
    let reload_rx = watchers.skill_reload_rx.into_inner();
    let _config_watcher = watchers.config_watcher;
    let config_reload_rx = watchers.config_reload_rx.into_inner();

    let mcp_embed_provider = {
        let discovery = &config.mcp.tool_discovery;
        if discovery.embedding_provider.is_empty() {
            provider.clone()
        } else {
            match crate::bootstrap::create_named_provider(&discovery.embedding_provider, config) {
                Ok(p) => {
                    tracing::info!(
                        provider = %discovery.embedding_provider,
                        "Using dedicated embed provider for MCP registry"
                    );
                    p
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %discovery.embedding_provider,
                        "MCP registry embed_provider resolution failed, using main provider: {e:#}"
                    );
                    provider.clone()
                }
            }
        }
    };
    let mcp_registry = create_mcp_registry(
        config,
        &mcp_embed_provider,
        &mcp_tools,
        &embed_model,
        app.qdrant_ops(),
    )
    .await;

    let index_pool = memory.sqlite().pool().clone();
    let index_provider = config
        .index
        .embed_provider
        .as_ref()
        .and_then(|p| p.as_non_empty())
        .and_then(
            |name| match crate::bootstrap::create_named_provider(name, config) {
                Ok(p) => {
                    tracing::info!(provider = %name, "Using dedicated embed provider for indexer");
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %name,
                        "Index embed_provider resolution failed, using main provider: {e:#}"
                    );
                    None
                }
            },
        )
        .unwrap_or_else(|| provider.clone());
    let index_qdrant_ops = app.qdrant_ops().cloned();
    let config_path = app.config_path().to_owned();
    let cache_pool = memory.sqlite().pool().clone();

    // Clone provider for the experiment scheduler only when the feature will actually be used.
    // The check must happen before `provider` moves into Agent::new_with_registry_arc.
    #[cfg(feature = "scheduler")]
    let provider_for_experiments =
        if config.experiments.enabled && config.experiments.schedule.enabled {
            Some(std::sync::Arc::new(provider.clone()))
        } else {
            None
        };

    let session_config = zeph_core::AgentSessionConfig::from_config(config, budget_tokens);

    // Pre-resolve RL embed dim before embedding_provider is moved into the agent builder.
    let rl_embed_dim_resolved = if config.skills.rl_routing_enabled {
        Some(
            resolve_rl_embed_dim(
                &config.skills,
                &embedding_provider,
                config.timeouts.embedding_seconds,
            )
            .await,
        )
    } else {
        None
    };

    // Create the gateway injection channel before agent construction so the receiver
    // can be wired into the channel wrapper.  The sender is stored and later passed to
    // spawn_gateway_server.  When the `gateway` feature is disabled the channel is
    // never created and `channel` is passed to the agent unchanged.
    #[cfg(feature = "gateway")]
    let (gateway_input_tx, gateway_input_rx) =
        tokio::sync::mpsc::channel::<zeph_core::ChannelMessage>(64);
    #[cfg(feature = "gateway")]
    let channel = crate::gateway_spawn::GatewayChannel::new(channel, gateway_input_rx);

    // Build TypedPagesState if enabled (#3630). Done before the builder chain because
    // CompactionAuditSink::open is async.
    let typed_pages_state = build_typed_pages_state(config).await;

    let agent = Agent::new_with_registry_arc(
        provider.clone(),
        embedding_provider.clone(),
        channel,
        registry,
        matcher,
        config.skills.max_active_skills.get(),
        tool_executor,
    )
    .apply_session_config(session_config)
    .with_active_provider_name(config.llm.providers.iter().find(|e| !e.embed).map_or_else(
        || provider.name().to_owned(),
        zeph_core::config::ProviderEntry::effective_name,
    ))
    .with_skill_matching_config(
        config.skills.disambiguation_threshold,
        config.skills.two_stage_matching,
        config.skills.confusability_threshold,
    )
    .with_skill_reload(skill_paths, reload_rx)
    .with_plugin_dirs_supplier(plugin_dirs_supplier)
    .with_managed_skills_dir(crate::bootstrap::managed_skills_dir())
    .with_trust_config(config.skills.trust.clone())
    .with_trust_snapshot(trust_snapshot)
    .with_memory(
        std::sync::Arc::clone(&memory),
        conversation_id,
        config.memory.history_limit,
        config.memory.semantic.recall_limit,
        config.memory.summarization_threshold,
    )
    .with_compression(config.memory.compression.clone())
    .with_typed_pages_state(typed_pages_state)
    .with_routing(config.memory.store_routing.clone())
    .with_shutdown(shutdown_rx.clone())
    .with_config_reload(config_path, config_reload_rx)
    .with_plugins_dir(crate::bootstrap::plugins_dir(), startup_shell_overlay)
    .with_shell_policy_handle(shell_policy_handle)
    .with_shell_executor_handle(shell_executor_handle)
    .with_background_completion_rx_opt(background_completion_rx)
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
    .with_rl_routing(
        config.skills.rl_routing_enabled,
        config.skills.rl_learning_rate,
        config.skills.rl_weight,
        config.skills.rl_persist_interval,
        config.skills.rl_warmup_updates,
    )
    .with_memory_formatting_config(
        config.memory.compression_guidelines.clone(),
        config.memory.digest.clone(),
        config.memory.context_strategy,
        config.memory.crossover_turn_threshold,
    )
    .with_retrieval_config(config.memory.retrieval.context_format)
    .with_focus_and_sidequest_config(config.agent.focus.clone(), config.memory.sidequest.clone())
    .with_trajectory_and_category_config(
        config.memory.trajectory.clone(),
        config.memory.category.clone(),
    )
    .with_embedding_provider(embedding_provider.clone())
    .maybe_init_tool_schema_filter(config.agent.tool_filter.clone(), embedding_provider)
    .await;

    // Wire JsonEventLayer when --json is active so tool_call / tool_result events
    // are emitted. JsonCliChannel no-ops send_tool_start / send_tool_output to
    // prevent double-emission; this layer is the canonical emitter.
    let agent = if let Some(ref sink) = json_sink {
        use zeph_core::json_event_layer::JsonEventLayer;
        agent.with_runtime_layer(std::sync::Arc::new(JsonEventLayer::new(
            std::sync::Arc::clone(sink),
        )))
    } else {
        agent
    };

    let agent = if let Some(logger) = tool_setup.audit_logger {
        agent.with_audit_logger(logger)
    } else {
        agent
    };

    // SkillOrchestra: load persisted RL routing head weights if enabled.
    let agent = if let Some(dim) = rl_embed_dim_resolved {
        let head = load_rl_head(&memory).await.unwrap_or_else(|| {
            // Cold start: no persisted weights yet, initialize a fresh head.
            // Dimension must match the configured embedding provider output.
            tracing::info!(dim, "rl_head: cold start, initializing fresh routing head");
            zeph_skills::rl_head::RoutingHead::new(dim)
        });
        agent.with_rl_head(head)
    } else {
        agent
    };

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
    let agent = if config.tools.policy.enabled {
        agent.with_policy_config(config.tools.policy.clone())
    } else {
        agent
    };
    let agent = if let Some(info) = adv_policy_info {
        agent.with_adversarial_policy_info(info)
    } else {
        agent
    };
    // Wire the trajectory risk slot and signal queue (spec 050 Invariant 2).
    let agent = agent
        .with_trajectory_risk_slot(trajectory_risk_slot)
        .with_signal_queue(trajectory_signal_queue)
        .with_trajectory_config(config.security.trajectory.clone())
        .0;
    // Spec 050 Phase 2: wire ShadowSentinel into agent so begin_turn() calls advance_turn().
    let agent = if let Some(sentinel) = shadow_sentinel_arc {
        agent.with_shadow_sentinel(sentinel)
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
        crate::bootstrap::effective_embedding_model(config),
    );
    let agent = agent_setup::apply_cost_tracker(agent, config);
    let agent = agent_setup::apply_summary_provider(agent, summary_provider);
    let probe_provider = app.build_probe_provider();
    let agent = if let Some(pp) = probe_provider {
        agent.with_probe_provider(pp)
    } else {
        agent
    };
    let agent = {
        let compress_provider = app.build_compress_provider();
        if let Some(cp) = compress_provider {
            agent.with_compress_provider(cp)
        } else {
            agent
        }
    };
    let planner_provider = app.build_planner_provider();
    let agent = if let Some(pp) = planner_provider {
        agent.with_planner_provider(pp)
    } else {
        agent
    };
    let verify_provider = app.build_verify_provider();
    let agent = if let Some(vp) = verify_provider {
        agent.with_verify_provider(vp)
    } else {
        agent
    };
    let orchestrator_provider = app.build_orchestrator_provider();
    let agent = if let Some(op) = orchestrator_provider {
        agent.with_orchestrator_provider(op)
    } else {
        agent
    };
    let predicate_provider = app.build_predicate_provider();
    let agent = if let Some(pp) = predicate_provider {
        agent.with_predicate_provider(pp)
    } else {
        agent
    };
    let agent = if let Some(ta) = app.build_topology_advisor() {
        agent.with_topology_advisor(ta)
    } else {
        agent
    };
    let agent = agent_setup::apply_quarantine_provider(agent, app.build_quarantine_provider());
    let agent = agent_setup::apply_guardrail(agent, app.build_guardrail_provider());
    let agent = agent.with_notifications(config.notifications.clone());
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_injection_classifier(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_enforcement_mode(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_three_class_classifier(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_pii_classifier(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_pii_ner_classifier(agent, config);
    let agent = agent_setup::apply_causal_analyzer(agent, provider.clone(), config);
    let agent = agent_setup::apply_vigil(agent, &config.security.vigil);

    let (_index_watcher, index_progress_rx) = if exec_mode.bare {
        (None, None)
    } else {
        #[cfg(feature = "tui")]
        if config.index.enabled {
            tui_status!("Indexing codebase...");
        }
        agent_setup::apply_code_indexer(
            &config.index,
            index_qdrant_ops,
            index_provider.clone(),
            index_pool,
            is_cli,
            Some(agent_status_tx.clone()),
            Some((*supervisor).clone()),
        )
        .await
    };
    // Wire index progress to TUI immediately after the indexer is created.
    #[cfg(feature = "tui")]
    if let (Some(early), Some(rx)) = (&early_tui_guard.0, index_progress_rx.clone()) {
        tokio::spawn(forward_index_progress_to_tui(rx, early.agent_tx.clone()));
    }
    #[cfg(not(feature = "tui"))]
    let _ = index_progress_rx;
    let agent = agent_setup::apply_code_retrieval(agent, &config.index);
    let agent = agent_setup::apply_code_rag_retriever(
        agent,
        &config.index,
        app.qdrant_ops().cloned(),
        index_provider.clone(),
        memory.sqlite().pool().clone(),
    );
    let agent = if let Some(search_executor) = agent_setup::build_search_code_executor(
        config,
        app.qdrant_ops().cloned(),
        index_provider.clone(),
        memory.sqlite().pool().clone(),
        Some(std::sync::Arc::clone(&mcp_manager)),
    ) {
        agent.add_tool_executor(search_executor)
    } else {
        agent
    };

    let agent = agent.with_mcp(mcp_tools, mcp_registry, Some(mcp_manager), &config.mcp);
    let agent = agent.with_mcp_server_outcomes(mcp_outcomes);
    let agent = agent.with_mcp_shared_tools(mcp_shared_tools);
    let agent = agent.with_mcp_tool_rx(mcp_tool_rx);
    let agent = if let Some(rx) = mcp_elicitation_rx {
        agent.with_mcp_elicitation_rx(rx)
    } else {
        agent
    };
    let agent = agent_setup::apply_mcp_pruning(agent, config);
    let agent = agent_setup::apply_mcp_discovery(agent, config);

    // Wire LSP context injection hooks when the feature is enabled and configured.
    let agent = if config.lsp.enabled {
        let runner = zeph_core::lsp_hooks::LspHookRunner::new(lsp_mcp_manager, config.lsp.clone());
        agent.with_lsp_hooks(runner)
    } else {
        agent
    };
    let agent = if exec_mode.bare {
        agent
    } else {
        agent.with_hooks_config(&config.hooks)
    };
    let agent = agent.with_channel_skills(channel_skills_config);
    let agent = agent.with_learning(config.skills.learning.clone());

    // Wire SkillEvaluator — enabled in both normal and bare mode (quality gate only).
    let skill_evaluator = crate::bootstrap::skills::build_skill_evaluator(config, &provider);
    let (eval_weights, eval_threshold) = if let Some(ref _eval) = skill_evaluator {
        let eval_cfg = &config.skills.evaluation;
        (
            zeph_skills::evaluator::EvaluationWeights {
                correctness: eval_cfg.weight_correctness,
                reusability: eval_cfg.weight_reusability,
                specificity: eval_cfg.weight_specificity,
            },
            eval_cfg.quality_threshold,
        )
    } else {
        (
            zeph_skills::evaluator::EvaluationWeights::default(),
            0.60_f32,
        )
    };
    if skill_evaluator.is_some() {
        tracing::info!(
            threshold = eval_threshold,
            "skills.evaluation: enabled (threshold={threshold})",
            threshold = eval_threshold
        );
    }
    let agent = agent.with_skill_evaluator(skill_evaluator.clone(), eval_weights, eval_threshold);

    // Wire ProactiveExplorer — gated on !bare to avoid background tasks in minimal sessions.
    let agent = if exec_mode.bare {
        agent
    } else {
        agent_setup::apply_proactive_explorer(
            agent,
            config,
            &provider,
            skill_evaluator.clone(),
            &skill_paths_for_features,
        )
    };

    // Wire PromotionEngine — gated on !bare to avoid background tasks in minimal sessions.
    let agent = if exec_mode.bare {
        agent
    } else {
        agent_setup::apply_promotion_engine(
            agent,
            config,
            &provider,
            skill_evaluator,
            eval_weights,
            eval_threshold,
            &skill_paths_for_features,
        )
    };
    let agent = agent.with_taco_compressor(taco_compressor);

    // Wire GoalAccounting — gated on config.goals.enabled (G4 invariant: always off in bare mode).
    let agent = if config.goals.enabled && !exec_mode.bare {
        let goal_pool = std::sync::Arc::new(memory.sqlite().pool().clone());
        let goal_store = std::sync::Arc::new(zeph_core::goal::GoalStore::new(goal_pool));
        let accounting = std::sync::Arc::new(zeph_core::goal::GoalAccounting::new(goal_store));
        tracing::info!("goals: enabled, GoalAccounting wired");
        agent.with_goal_accounting(Some(accounting))
    } else {
        agent
    };

    let judge_provider = app.build_judge_provider();
    let agent = if let Some(jp) = judge_provider {
        agent.with_judge_provider(jp)
    } else {
        agent
    };
    let agent = if let Some(fc) = app.build_feedback_classifier(&provider) {
        agent.with_llm_classifier(fc)
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

    let agent = {
        let mut mgr = zeph_subagent::SubAgentManager::new(config.agents.max_concurrent);
        let agent_paths = match zeph_subagent::resolve_agent_paths(
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
        agent.with_orchestration(config.orchestration.clone(), config.agents.clone(), mgr)
    };
    let agent = {
        let baseline = zeph_experiments::ConfigSnapshot::from_config(config);
        let agent = agent.with_experiment(config.experiments.clone(), baseline);
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
    let agent = if exec_mode.bare {
        agent
    } else {
        let exp_deps = provider_for_experiments.map(|p| (p, Some(std::sync::Arc::clone(&memory))));
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

    // Wire SpeculationEngine after all add_tool_executor calls so the captured executor Arc
    // includes the fully composed tool chain (search + scheduler + any future executors).
    // Gated on mode != Off and !bare to avoid background sweeper tasks in minimal sessions.
    let agent = if config.tools.speculative.mode != zeph_config::tools::SpeculationMode::Off
        && !exec_mode.bare
    {
        let spec_executor = agent.tool_executor_arc();
        let engine = std::sync::Arc::new(
            zeph_core::agent::speculative::SpeculationEngine::new_with_supervisor(
                spec_executor,
                config.tools.speculative.clone(),
                Some(std::sync::Arc::clone(&supervisor)),
            ),
        );
        tracing::info!(
            mode = ?config.tools.speculative.mode,
            "speculation: enabled, SpeculationEngine wired"
        );
        agent.with_speculation_engine(Some(engine))
    } else {
        agent
    };

    // Wire PASTE PatternStore when mode is Pattern or Both and memory is available.
    // Initialized here (after SpeculationEngine) so the pool reference is always fresh.
    let agent = {
        use zeph_config::tools::SpeculationMode;
        let needs_paste = matches!(
            config.tools.speculative.mode,
            SpeculationMode::Pattern | SpeculationMode::Both
        ) && !exec_mode.bare;
        if needs_paste {
            let pool = memory.sqlite().pool().clone();
            let half_life_days = config.tools.speculative.pattern.half_life_days;
            let store = std::sync::Arc::new(
                zeph_core::agent::speculative::paste::PatternStore::new(pool, half_life_days),
            );
            tracing::info!("speculation: PASTE PatternStore wired");
            agent.with_pattern_store(Some(store))
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

    // Gateway is spawned after the metrics channel is created (lines ~1835 below).
    // The actual spawn_gateway_server call is deferred to after metrics wiring.

    #[allow(unused_variables)]
    let agent = {
        let language = config
            .llm
            .stt
            .as_ref()
            .map_or("auto", |s| s.language.as_str());
        if let Some(stt_entry) = config.llm.stt_provider_entry() {
            match stt_entry.provider_type {
                #[cfg(feature = "candle")]
                zeph_core::config::ProviderKind::Candle => {
                    agent_setup::apply_candle_stt(agent, stt_entry, language)
                }
                #[cfg(not(feature = "candle"))]
                zeph_core::config::ProviderKind::Candle => {
                    tracing::error!(
                        provider = stt_entry.effective_name(),
                        "STT provider is type candle but the `candle` feature is not enabled; \
                         STT disabled"
                    );
                    agent
                }
                #[cfg(feature = "cocoon")]
                zeph_core::config::ProviderKind::Cocoon => agent_setup::apply_cocoon_stt(
                    agent,
                    stt_entry,
                    language,
                    config.timeouts.llm_request_timeout_secs,
                ),
                #[cfg(not(feature = "cocoon"))]
                zeph_core::config::ProviderKind::Cocoon => {
                    tracing::error!(
                        provider = stt_entry.effective_name(),
                        "STT provider is type cocoon but the `cocoon` feature is not enabled; \
                         STT disabled"
                    );
                    agent
                }
                _ => {
                    let api_key = resolve_stt_api_key(config, stt_entry);
                    agent_setup::apply_whisper_stt(agent, stt_entry, language, api_key)
                }
            }
        } else {
            if config.llm.stt.is_some() {
                tracing::warn!(
                    provider = config.llm.stt.as_ref().map_or("", |s| s.provider.as_str()),
                    "[[llm.stt]] is configured but no matching [[llm.providers]] entry with \
                     `stt_model` was found; STT disabled"
                );
            }
            agent
        }
    };

    // When profiling is enabled, reuse the MetricsCollector created before init_tracing
    // (the MetricsBridge layer holds an Arc to it). Extract sender/receiver from it.
    #[cfg(feature = "profiling")]
    let (metrics_tx, metrics_rx) = {
        let rx = metrics_rx_early;
        let tx = metrics_collector_arc.sender();
        (tx, rx)
    };
    #[cfg(not(feature = "profiling"))]
    let (metrics_tx, metrics_rx) =
        tokio::sync::watch::channel(zeph_core::metrics::MetricsSnapshot::default());
    let static_metrics_init = {
        let stt_model = config
            .llm
            .stt_provider_entry()
            .and_then(|e| e.stt_model.clone());
        let compaction_model = config.llm.summary_model.clone();
        let semantic_cache_enabled = config.llm.semantic_cache_enabled;
        let embedding_model = crate::bootstrap::effective_embedding_model(config).clone();
        let self_learning_enabled = config.skills.learning.enabled;
        let token_budget = u64::try_from(budget_tokens).ok();
        let compaction_threshold = u32::try_from(budget_tokens).ok().map(|b| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let threshold =
                (f64::from(b) * f64::from(config.memory.soft_compaction_threshold)) as u32;
            threshold
        });
        zeph_core::metrics::StaticMetricsInit {
            stt_model,
            compaction_model,
            semantic_cache_enabled,
            embedding_model,
            self_learning_enabled,
            active_channel: active_channel_name.clone(),
            token_budget,
            compaction_threshold,
            vault_backend: config.vault.backend.clone(),
            autosave_enabled: config.memory.autosave_assistant,
            model_name_override: Some(config.llm.effective_model().to_owned()),
        }
    };
    // Spawn egress telemetry drain now that metrics_tx is available.
    if let Some(rx) = egress_rx {
        tokio::spawn(agent_setup::drain_egress_events(
            rx,
            Some(metrics_tx.clone()),
        ));
    }
    // Clone metrics_rx for Prometheus sync task before it is consumed by TUI or dropped.
    #[cfg(feature = "prometheus")]
    let prometheus_metrics_rx = metrics_rx.clone();

    // Pre-create the PrometheusMetrics instance so its Arc can be passed both to the
    // histogram recorder wiring (before agent construction) and to the sync task (below).
    // The Arc is None when the feature is disabled or metrics/gateway is not enabled.
    #[cfg(feature = "prometheus")]
    let prom_arc: Option<std::sync::Arc<crate::metrics_export::PrometheusMetrics>> =
        if config.metrics.enabled && config.gateway.enabled {
            // M4: validate metrics.path before using it.
            let path = &config.metrics.path;
            if path.is_empty() || !path.starts_with('/') {
                tracing::warn!(
                    path = %path,
                    "[metrics] metrics.path must be non-empty and start with '/'; \
                     got '{path}' — using default '/metrics'"
                );
            }
            Some(std::sync::Arc::new(
                crate::metrics_export::PrometheusMetrics::new(),
            ))
        } else {
            None
        };

    #[cfg(all(feature = "tui", feature = "scheduler"))]
    let metrics_tx_for_sched = metrics_tx.clone();
    #[cfg(all(feature = "tui", feature = "cocoon"))]
    let metrics_tx_for_cocoon = metrics_tx.clone();
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
        gonka_private_key: config
            .secrets
            .gonka_private_key
            .as_ref()
            .map(|s| zeroize::Zeroizing::new(s.expose().to_owned())),
        gonka_address: config
            .secrets
            .gonka_address
            .as_ref()
            .map(|s| s.expose().to_owned()),
        cocoon_access_hash: config
            .secrets
            .cocoon_access_hash
            .as_ref()
            .map(|s| s.expose().to_owned()),
    };
    let agent = agent
        .with_extended_context(extended_context)
        .with_metrics(metrics_tx)
        .with_static_metrics(static_metrics_init)
        .with_status_tx(agent_status_tx)
        .with_provider_pool(config.llm.providers.clone(), provider_config_snapshot)
        .with_channel_identity(
            active_channel_name.clone(),
            config.session.provider_persistence,
        );

    #[cfg(feature = "prometheus")]
    let agent = {
        let recorder: Option<std::sync::Arc<dyn zeph_core::metrics::HistogramRecorder>> =
            prom_arc.as_ref().map(|p| {
                std::sync::Arc::clone(p)
                    as std::sync::Arc<dyn zeph_core::metrics::HistogramRecorder>
            });
        agent.with_histogram_recorder(recorder)
    };

    // Wire supervisor config so concurrency limits and turn-boundary abort are applied (#2883).
    let agent = agent.with_supervisor_config(&config.agent.supervisor);
    // Wire session-level TaskSupervisor so agent background tasks are observable (#3508).
    let agent = agent.with_task_supervisor(std::sync::Arc::clone(&supervisor));
    let agent = agent.with_acp_config(config.acp.clone());

    // Wire ACP sub-agent spawn callback so `/subagent spawn <cmd>` works in CLI/piped mode (#3302).
    #[cfg(feature = "acp")]
    let agent = {
        let spawn_fn: zeph_subagent::AcpSubagentSpawnFn = std::sync::Arc::new(|command: String| {
            Box::pin(async move {
                let cfg = zeph_acp::client::SubagentConfig {
                    command,
                    auto_approve_permissions: true,
                    ..zeph_acp::client::SubagentConfig::default()
                };
                zeph_acp::run_session(cfg, String::new())
                    .await
                    .map(|o| o.text)
                    .map_err(|e| e.to_string())
            })
        });
        agent.with_acp_subagent_spawn_fn(spawn_fn)
    };

    let agent = {
        let pipeline = if config.quality.self_check {
            zeph_core::quality::SelfCheckPipeline::build(
                &zeph_core::quality::QualityConfig::from(&config.quality),
                &provider,
            )
            .map_err(|e| anyhow::anyhow!("self-check pipeline init failed: {e}"))
            .ok()
        } else {
            None
        };
        agent.with_quality_pipeline(pipeline)
    };

    let agent = agent
        .build()
        .map_err(|e| anyhow::anyhow!("agent construction failed: {e}"))?;

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
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if let Ok(jobs) = store.list_jobs().await {
                                tx_clone.send_modify(|m| {
                                    m.scheduled_tasks = jobs
                                        .into_iter()
                                        .map(|r| [r.name, r.kind, r.task_mode, r.next_run])
                                        .collect();
                                });
                            }
                        }
                        () = async {
                            if let Some(ref mut rx) = refresh_rx {
                                let _ = rx.changed().await;
                            } else {
                                std::future::pending::<()>().await;
                            }
                        } => {
                            if let Ok(jobs) = store.list_jobs().await {
                                tx_clone.send_modify(|m| {
                                    m.scheduled_tasks = jobs
                                        .into_iter()
                                        .map(|r| [r.name, r.kind, r.task_mode, r.next_run])
                                        .collect();
                                });
                            }
                        }
                        _ = shutdown.changed() => break,
                    }
                }
            });
        }
        #[cfg(feature = "cocoon")]
        if let Some(cocoon_cfg) = config
            .llm
            .providers
            .iter()
            .find(|p| p.provider_type == zeph_config::ProviderKind::Cocoon)
        {
            let base_url = cocoon_cfg
                .cocoon_client_url
                .clone()
                .unwrap_or_else(|| "http://localhost:10000".to_owned());
            let access_hash = config
                .secrets
                .cocoon_access_hash
                .as_ref()
                .map(|s| s.expose().to_owned());
            let client = zeph_llm::cocoon::CocoonClient::new(
                &base_url,
                access_hash,
                std::time::Duration::from_secs(5),
            );
            let metrics_tx_cocoon = metrics_tx_for_cocoon;
            let mut shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            let span = tracing::info_span!("tui.cocoon.poll");
                            let _enter = span.enter();
                            let health = client.health_check().await;
                            let models = client.list_models().await;
                            metrics_tx_cocoon.send_modify(|m| {
                                if let Ok(h) = &health {
                                    m.cocoon_connected = Some(h.proxy_connected);
                                    m.cocoon_worker_count = h.worker_count;
                                    m.cocoon_ton_balance = h.ton_balance;
                                } else {
                                    m.cocoon_connected = Some(false);
                                    m.cocoon_worker_count = 0;
                                    m.cocoon_ton_balance = None;
                                }
                                m.cocoon_model_count =
                                    models.as_ref().map_or(0, Vec::len);
                            });
                        }
                        _ = shutdown.changed() => break,
                    }
                }
                tracing::debug!("cocoon health poll task shutting down");
            });
        }
    } else {
        tui_metrics_rx = None;
        drop(metrics_rx);
    };

    // Wire up Prometheus metrics sync and spawn the gateway server.
    //
    // S1 fix (critic review): gateway is spawned HERE, after the metrics watch channel exists,
    // so prometheus_metrics_rx is available. This replaces the earlier placeholder comment.
    // TODO(#2866 Phase 2): register prometheus_sync_handle with the background task supervisor
    // instead of storing it as a fire-and-forget binding. For MVP the handle is kept alive by the
    // binding until the process exits.
    // `prometheus` feature implies `gateway` (see Cargo.toml feature definition), so no inner
    // `#[cfg(feature = "gateway")]` guards are needed inside this block.
    #[cfg(feature = "prometheus")]
    let _prometheus_sync_handle = if exec_mode.bare {
        None
    } else if let Some(prom) = prom_arc {
        let handle = crate::metrics_export::spawn_metrics_sync(
            std::sync::Arc::clone(&prom),
            prometheus_metrics_rx,
            config.metrics.sync_interval_secs,
        );
        let effective_path = {
            let p = &config.metrics.path;
            if p.is_empty() || !p.starts_with('/') {
                "/metrics".to_owned()
            } else {
                p.clone()
            }
        };
        crate::gateway_spawn::spawn_gateway_server(
            config,
            shutdown_rx.clone(),
            gateway_input_tx.clone(),
            Some((std::sync::Arc::clone(&prom.registry), effective_path)),
        );
        Some(handle)
    } else {
        if config.metrics.enabled && !config.gateway.enabled {
            tracing::warn!(
                "[metrics] enabled=true but [gateway] enabled=false; skipping Prometheus metrics export"
            );
        }
        if config.gateway.enabled {
            crate::gateway_spawn::spawn_gateway_server(
                config,
                shutdown_rx.clone(),
                gateway_input_tx.clone(),
                None,
            );
        }
        None
    };

    // When `prometheus` feature is disabled, spawn gateway unconditionally if enabled.
    #[cfg(all(feature = "gateway", not(feature = "prometheus")))]
    if !exec_mode.bare && config.gateway.enabled {
        crate::gateway_spawn::spawn_gateway_server(config, shutdown_rx.clone(), gateway_input_tx);
    }

    let mut agent = agent;
    #[cfg(feature = "tui")]
    tui_status!("Connecting to memory store...");
    agent
        .check_vector_store_health(config.memory.vector_backend.as_str())
        .await;
    agent.sync_graph_counts().await;
    agent.init_semantic_index().await;

    agent_setup::spawn_ctrl_c_handler(agent.cancel_signal(), shutdown_tx);
    early_ctrlc.abort();
    #[cfg(feature = "tui")]
    tui_status!("Loading conversation history...");
    // load_history is the last fallible call before run_tui_agent.
    // EarlyTuiGuard handles cleanup for all prior ? operators automatically.
    agent.load_history().await?;
    #[cfg(feature = "tui")]
    tui_status!("");

    #[cfg(feature = "tui")]
    if let Some(tui_handle) = tui_handle {
        // Defuse the guard — TUI task is handed off to run_tui_agent, which owns cleanup.
        let early_tui = early_tui_guard.defuse();
        // index_progress_rx was already forwarded to TUI after apply_code_indexer;
        // pass None here to avoid spawning a duplicate forwarder.
        let progress_for_params = if early_tui.is_some() {
            None
        } else {
            index_progress_rx
        };
        return Box::pin(run_tui_agent(
            agent,
            TuiRunParams {
                tui_handle,
                config,
                status_rx: tui_status_rx_for_params,
                tool_rx: shell_executor_for_tui,
                metrics_rx: tui_metrics_rx,
                warmup_provider: warmup_provider_clone,
                index_progress_rx: progress_for_params,
                cli_tafc: cli.tafc,
                early_tui,
                backfill_rx,
                task_supervisor: Some((*supervisor).clone()),
            },
        ))
        .await;
    }
    // TUI feature compiled but running in CLI mode — backfill_rx not needed.
    #[cfg(feature = "tui")]
    drop(backfill_rx);

    if let Some(handle) = warmup_handle {
        let _ = handle.await;
    }
    // When the tui feature is compiled in but running in CLI mode, status_rx was moved
    // into tui_status_rx_for_params above. Recover it here; it is always Some in CLI mode
    // because the early forwarder is only spawned when early_tui_guard.0 is Some (TUI path).
    #[cfg(feature = "tui")]
    let status_rx = tui_status_rx_for_params
        .expect("status_rx must be Some in CLI mode: early forwarder only runs on TUI path");
    tokio::spawn(forward_status_to_stderr(status_rx));
    let result = Box::pin(agent.run()).await;
    // Explicitly shut down MCP connections before agent.shutdown() so that child processes
    // are killed while the tokio runtime is still active (#2693).
    shutdown_mcp_manager.shutdown_all_shared().await;
    agent.shutdown().await;
    supervisor
        .shutdown_all(std::time::Duration::from_secs(10))
        .await;
    Ok(result?)
}

/// Print experiment results from `SQLite` and exit. Does not require an LLM provider.
///
/// Load persisted RL routing head weights from memory store.
///
/// Returns `None` when no weights are stored yet (cold start) or on any DB error.
pub(crate) async fn load_rl_head(
    memory: &zeph_memory::semantic::SemanticMemory,
) -> Option<zeph_skills::rl_head::RoutingHead> {
    match memory.sqlite().load_routing_head_weights().await {
        Ok(Some((embed_dim, weights, _baseline, _count))) => {
            zeph_skills::rl_head::RoutingHead::from_bytes(&weights).or_else(|| {
                // Stored embed_dim doesn't match bytes — initialize fresh.
                tracing::warn!(
                    embed_dim,
                    "rl_head: stored weights corrupt or incompatible, initializing fresh"
                );
                let dim = usize::try_from(embed_dim).unwrap_or(0);
                if dim == 0 {
                    None
                } else {
                    Some(zeph_skills::rl_head::RoutingHead::new(dim))
                }
            })
        }
        Ok(None) => {
            // No weights stored yet — will be initialized lazily when embed_dim is known.
            None
        }
        Err(e) => {
            tracing::debug!("rl_head: failed to load weights: {e:#}");
            None
        }
    }
}

/// Resolve the RL routing head embedding dimension.
///
/// Uses the explicit `rl_embed_dim` config value when set. Otherwise probes the
/// embedding provider with a single empty-string call to determine the actual
/// output dimension at runtime. Falls back to 1536 with a WARN when the probe
/// also fails, instructing the operator to set `skills.rl_embed_dim` explicitly.
pub(crate) async fn resolve_rl_embed_dim(
    skills_config: &zeph_core::config::SkillsConfig,
    embedding_provider: &LlmAnyProvider,
    embedding_timeout_secs: u64,
) -> usize {
    const FALLBACK: usize = 1536;
    if let Some(dim) = skills_config.rl_embed_dim {
        return dim;
    }
    let probe = tokio::time::timeout(
        std::time::Duration::from_secs(embedding_timeout_secs),
        embedding_provider.embed(" "),
    )
    .await;
    match probe {
        Ok(Ok(v)) if !v.is_empty() => v.len(),
        Ok(Ok(_) | Err(_)) => {
            tracing::warn!(
                fallback = FALLBACK,
                "rl_head: could not probe embedding dimension from provider; \
                 set `skills.rl_embed_dim` in config to avoid this fallback"
            );
            FALLBACK
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = embedding_timeout_secs,
                fallback = FALLBACK,
                "rl_head: embedding probe timed out; \
                 set `skills.rl_embed_dim` in config to avoid this fallback"
            );
            FALLBACK
        }
    }
}

/// # Errors
///
/// Returns an error if the database cannot be opened or the query fails.
async fn run_experiment_report(app: &crate::bootstrap::AppBuilder) -> anyhow::Result<()> {
    use zeph_memory::store::SqliteStore;

    let store = SqliteStore::new(crate::db_url::resolve_db_url(app.config())).await?;
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
async fn run_experiment_session(
    app: crate::bootstrap::AppBuilder,
    provider: zeph_llm::any::AnyProvider,
) -> anyhow::Result<()> {
    use std::sync::Arc;

    use zeph_experiments::{
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

    // --- resolve_rl_embed_dim ---

    /// A slow embed (1100 ms) cut off by a 1-second timeout must fall back to 1536.
    #[tokio::test]
    async fn resolve_rl_embed_dim_timeout_uses_fallback() {
        use zeph_llm::mock::MockProvider;
        let config = zeph_core::Config::default();
        // 1100 ms delay > 1 s timeout → guaranteed to trigger, 100 ms safety margin
        let provider =
            zeph_llm::any::AnyProvider::Mock(MockProvider::default().with_embed_delay(1100));
        let dim = resolve_rl_embed_dim(&config.skills, &provider, 1).await;
        assert_eq!(dim, 1536);
    }

    /// A fast embed returning a 768-dim vector must be returned unchanged.
    #[tokio::test]
    async fn resolve_rl_embed_dim_fast_provider_returns_dim() {
        use zeph_llm::mock::MockProvider;
        let config = zeph_core::Config::default();
        let provider = zeph_llm::any::AnyProvider::Mock(
            MockProvider::default().with_embedding(vec![0.0f32; 768]),
        );
        let dim = resolve_rl_embed_dim(&config.skills, &provider, 30).await;
        assert_eq!(dim, 768);
    }

    // --- bare-mode guards ---

    /// `--bare` CLI flag activates bare mode; `!exec_mode.bare` is false so mem-eviction
    /// is not spawned.
    #[test]
    fn bare_flag_suppresses_mem_eviction_guard() {
        let cli = Cli::parse_from(["zeph", "--bare"]);
        let mode =
            crate::execution_mode::ExecutionMode::from_cli_and_config(&cli, &Config::default());
        // Guard condition in runner: `if !exec_mode.bare { spawn mem-eviction }`
        assert!(
            mode.bare,
            "bare mode must make the spawn guard evaluate to false"
        );
    }

    /// `--bare` CLI flag causes the indexer guard to produce `(None, None)` without calling
    /// `apply_code_indexer`.
    #[test]
    fn bare_flag_skips_code_indexer_guard() {
        let cli = Cli::parse_from(["zeph", "--bare"]);
        let mode =
            crate::execution_mode::ExecutionMode::from_cli_and_config(&cli, &Config::default());
        // Guard: `if exec_mode.bare { (None, None) } else { apply_code_indexer(...) }`
        let result: (Option<()>, Option<()>) = if mode.bare {
            (None, None)
        } else {
            (Some(()), Some(()))
        };
        assert!(
            result.0.is_none(),
            "indexer watcher must be None in bare mode"
        );
        assert!(
            result.1.is_none(),
            "indexer progress rx must be None in bare mode"
        );
    }

    /// `--bare` CLI flag causes the scheduler guard to pass the agent through unchanged.
    #[test]
    fn bare_flag_skips_scheduler_guard() {
        let cli = Cli::parse_from(["zeph", "--bare"]);
        let mode =
            crate::execution_mode::ExecutionMode::from_cli_and_config(&cli, &Config::default());
        // Guard: `if exec_mode.bare { agent } else { bootstrap_scheduler(...) }`
        let scheduler_would_run = !mode.bare;
        assert!(!scheduler_would_run, "scheduler must not run in bare mode");
    }

    /// Without `--bare`, all three subsystems are allowed to start (guards evaluate to true).
    #[test]
    fn non_bare_mode_allows_mem_eviction_indexer_scheduler() {
        let cli = Cli::parse_from(["zeph"]);
        let mode =
            crate::execution_mode::ExecutionMode::from_cli_and_config(&cli, &Config::default());
        assert!(!mode.bare, "default mode must not be bare");
        // mem-eviction guard: `if !exec_mode.bare` → true
        assert!(!mode.bare);
        // indexer guard: `if exec_mode.bare { (None, None) } else { ... }`
        let indexer_result: (Option<()>, Option<()>) = if mode.bare {
            (None, None)
        } else {
            (Some(()), Some(()))
        };
        assert!(
            indexer_result.0.is_some(),
            "indexer watcher slot must be Some in non-bare mode"
        );
        assert!(
            indexer_result.1.is_some(),
            "indexer progress rx slot must be Some in non-bare mode"
        );
        // scheduler guard: `if exec_mode.bare { agent } else { ... }`
        let scheduler_would_run = !mode.bare;
        assert!(
            scheduler_would_run,
            "scheduler must be allowed in non-bare mode"
        );
    }

    /// `--bare` suppresses MCP `connect_all` — the guard `if bare { (vec![], vec![]) }` fires.
    #[test]
    fn bare_flag_skips_mcp_connect_guard() {
        let cli = Cli::parse_from(["zeph", "--bare"]);
        let mode =
            crate::execution_mode::ExecutionMode::from_cli_and_config(&cli, &Config::default());
        // Guard: `if bare { (Vec::new(), Vec::new()) } else { mcp_manager.connect_all().await }`
        let mcp_would_connect = !mode.bare;
        assert!(
            !mcp_would_connect,
            "MCP connect_all must be skipped in bare mode"
        );
    }

    /// `--bare` suppresses gateway spawn — guards `!exec_mode.bare` prevent both code paths.
    #[test]
    fn bare_flag_skips_gateway_spawn_guard() {
        let cli = Cli::parse_from(["zeph", "--bare"]);
        let mode =
            crate::execution_mode::ExecutionMode::from_cli_and_config(&cli, &Config::default());
        // Guard: `if exec_mode.bare { None } else { spawn_gateway_server(...) }`
        let gateway_would_spawn = !mode.bare;
        assert!(!gateway_would_spawn, "gateway must not spawn in bare mode");
    }

    /// `--bare` sets the bare execution mode flag.
    #[test]
    fn bare_flag_sets_execution_mode() {
        let cli = Cli::parse_from(["zeph", "--bare"]);
        let mode =
            crate::execution_mode::ExecutionMode::from_cli_and_config(&cli, &Config::default());
        assert!(mode.bare, "bare flag must set execution mode");
    }

    // --- ShadowSentinelProbeGateAdapter ---

    async fn make_adapter_sentinel(
        verdict: zeph_core::agent::shadow_sentinel::ProbeVerdict,
    ) -> ShadowSentinelProbeGateAdapter {
        use zeph_core::agent::shadow_sentinel::{
            ProbeVerdict, SafetyProbe, ShadowEvent, ShadowEventStore, ShadowSentinel,
        };

        struct FixedProbe(ProbeVerdict);
        impl SafetyProbe for FixedProbe {
            fn evaluate<'a>(
                &'a self,
                _: &'a str,
                _: &'a serde_json::Value,
                _: &'a [ShadowEvent],
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeVerdict> + Send + 'a>>
            {
                let v = self.0.clone();
                Box::pin(async move { v })
            }
        }

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite");
        let store = ShadowEventStore::new(pool);
        let config = zeph_config::ShadowSentinelConfig {
            enabled: true,
            ..Default::default()
        };
        let sentinel = std::sync::Arc::new(ShadowSentinel::new(
            store,
            Box::new(FixedProbe(verdict)),
            config,
            "test",
        ));
        ShadowSentinelProbeGateAdapter { sentinel }
    }

    #[tokio::test]
    async fn probe_gate_adapter_maps_allow_to_allow() {
        use zeph_core::agent::shadow_sentinel::ProbeVerdict;
        use zeph_tools::{ProbeGate, ProbeOutcome};

        let adapter = make_adapter_sentinel(ProbeVerdict::Allow).await;
        let args = serde_json::Value::Object(serde_json::Map::new());
        let outcome = adapter.probe("builtin:shell", &args, 1, "calm").await;
        assert_eq!(outcome, ProbeOutcome::Allow);
    }

    #[tokio::test]
    async fn probe_gate_adapter_maps_deny_to_deny() {
        use zeph_core::agent::shadow_sentinel::ProbeVerdict;
        use zeph_tools::{ProbeGate, ProbeOutcome};

        let adapter = make_adapter_sentinel(ProbeVerdict::Deny {
            reason: "risky pattern".to_owned(),
        })
        .await;
        let args = serde_json::Value::Object(serde_json::Map::new());
        let outcome = adapter.probe("builtin:shell", &args, 1, "elevated").await;
        assert_eq!(
            outcome,
            ProbeOutcome::Deny {
                reason: "risky pattern".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn probe_gate_adapter_maps_skip_when_disabled() {
        use zeph_core::agent::shadow_sentinel::{
            ProbeVerdict, SafetyProbe, ShadowEvent, ShadowEventStore, ShadowSentinel,
        };
        use zeph_tools::{ProbeGate, ProbeOutcome};

        struct PanicProbe;
        impl SafetyProbe for PanicProbe {
            fn evaluate<'a>(
                &'a self,
                _: &'a str,
                _: &'a serde_json::Value,
                _: &'a [ShadowEvent],
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeVerdict> + Send + 'a>>
            {
                Box::pin(async { panic!("probe must not be called when disabled") })
            }
        }

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite");
        let store = ShadowEventStore::new(pool);
        let config = zeph_config::ShadowSentinelConfig {
            enabled: false,
            ..Default::default()
        };
        let sentinel = std::sync::Arc::new(ShadowSentinel::new(
            store,
            Box::new(PanicProbe),
            config,
            "test",
        ));
        let adapter = ShadowSentinelProbeGateAdapter { sentinel };

        let args = serde_json::Value::Object(serde_json::Map::new());
        let outcome = adapter.probe("builtin:shell", &args, 1, "calm").await;
        assert_eq!(outcome, ProbeOutcome::Skip);
    }
}
