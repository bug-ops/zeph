// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use dialoguer::{Confirm, Input, Password, Select};
use zeph_core::config::{
    AcpConfig, ChannelSkillsConfig, Config, DiscordConfig, LlmConfig, LlmRoutingStrategy,
    McpServerConfig, McpTrustLevel, MemoryConfig, OrchestrationConfig, ProviderEntry, ProviderKind,
    ProviderName, PruningStrategy, SemanticConfig, SessionsConfig, SlackConfig, TelegramConfig,
    VaultConfig,
};
use zeph_llm::{GeminiThinkingLevel, ThinkingConfig};
use zeph_subagent::def::{MemoryScope, PermissionMode};

pub(super) mod agents;
pub(super) mod llm;
pub(super) mod mcp;
pub(super) mod memory;
pub(super) mod security;

use agents::{step_agents, step_learning, step_orchestration, step_router};
use llm::step_llm;
use mcp::{step_mcp_discovery, step_mcp_remote, step_mcpls, write_mcpls_config};
use memory::{step_context_compression, step_memory};
use security::{step_policy, step_sandbox, step_security};

#[cfg_attr(test, derive(Clone))]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct WizardState {
    pub(crate) provider: Option<ProviderKind>,
    pub(crate) base_url: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) embedding_model: Option<String>,
    pub(crate) vision_model: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) compatible_name: Option<String>,
    pub(crate) sqlite_path: Option<String>,
    pub(crate) sessions_max_history: usize,
    pub(crate) sessions_title_max_chars: usize,
    pub(crate) qdrant_url: Option<String>,
    pub(crate) semantic_enabled: bool,
    pub(crate) channel: ChannelChoice,
    pub(crate) telegram_token: Option<String>,
    pub(crate) telegram_users: Vec<String>,
    pub(crate) discord_token: Option<String>,
    pub(crate) discord_app_id: Option<String>,
    pub(crate) slack_bot_token: Option<String>,
    pub(crate) slack_signing_secret: Option<String>,
    pub(crate) vault_backend: String,
    pub(crate) auto_update_check: bool,
    pub(crate) scheduler_enabled: bool,
    pub(crate) scheduler_tick_interval_secs: u64,
    pub(crate) scheduler_max_tasks: usize,
    pub(crate) daemon_enabled: bool,
    pub(crate) daemon_host: String,
    pub(crate) daemon_port: u16,
    pub(crate) daemon_auth_token: Option<String>,
    pub(crate) acp_enabled: bool,
    pub(crate) acp_agent_name: String,
    pub(crate) acp_agent_version: String,
    pub(crate) thinking: Option<ThinkingConfig>,
    pub(crate) enable_extended_context: bool,
    pub(crate) agents_default_permission_mode: Option<PermissionMode>,
    pub(crate) agents_default_disallowed_tools: Vec<String>,
    pub(crate) agents_allow_bypass_permissions: bool,
    /// Custom user-level agents directory (empty = use platform default).
    pub(crate) agents_user_dir: Option<std::path::PathBuf>,
    /// Default memory scope for sub-agents (None = no memory by default).
    pub(crate) agents_default_memory_scope: Option<MemoryScope>,
    /// "regex", "judge", or "model" — defaults to "regex" (no LLM calls).
    pub(crate) detector_mode: Option<String>,
    pub(crate) judge_model: Option<String>,
    /// Provider name from `[[llm.providers]]` for `DetectorMode::Model`. Empty = primary.
    pub(crate) feedback_provider: Option<String>,
    /// Router strategy: None = no router, "ema", "thompson", or "cascade".
    pub(crate) router_strategy: Option<String>,
    /// Custom path for Thompson state file (None = use default).
    pub(crate) router_thompson_state_path: Option<String>,
    /// Cascade: minimum quality score to accept without escalating (default 0.5).
    pub(crate) router_cascade_quality_threshold: Option<f64>,
    /// Cascade: maximum number of quality-based escalations per request (default 2).
    pub(crate) router_cascade_max_escalations: Option<u8>,
    /// Cascade: explicit cost ordering of provider names (cheapest first). None = chain order.
    pub(crate) router_cascade_cost_tiers: Option<Vec<String>>,
    // Orchestration settings
    pub(crate) orchestration_enabled: bool,
    pub(crate) orchestration_max_tasks: u32,
    pub(crate) orchestration_max_parallel: u32,
    pub(crate) orchestration_confirm_before_execute: bool,
    pub(crate) orchestration_failure_strategy: String,
    pub(crate) orchestration_planner_provider: Option<String>,
    pub(crate) orchestration_persistence_enabled: bool,
    // Debug settings
    pub(crate) debug_dump_enabled: bool,
    pub(crate) debug_dump_format: zeph_core::debug_dump::DumpFormat,
    // Graph memory settings
    pub(crate) graph_memory_enabled: bool,
    pub(crate) graph_extract_model: Option<String>,
    pub(crate) graph_spreading_activation_enabled: bool,
    // ACON failure-driven compression guidelines
    pub(crate) compression_guidelines_enabled: bool,
    // Context compression: Focus Agent + SideQuest + pruning strategy
    pub(crate) focus_enabled: bool,
    pub(crate) focus_compression_interval: usize,
    pub(crate) sidequest_enabled: bool,
    pub(crate) sidequest_interval_turns: u32,
    pub(crate) pruning_strategy: String,
    // AOI three-layer memory tiers
    pub(crate) memory_tiers_enabled: bool,
    pub(crate) memory_tiers_promotion_min_sessions: u32,
    // Server-side compaction
    pub(crate) gemini_thinking_level: Option<GeminiThinkingLevel>,
    pub(crate) server_compaction_enabled: bool,
    // LSP code intelligence via mcpls
    pub(crate) mcpls_enabled: bool,
    pub(crate) mcpls_workspace_roots: Vec<String>,
    // Remote MCP servers with OAuth or static headers
    pub(crate) mcp_remote_servers: Vec<McpServerConfig>,
    // LSP context injection
    pub(crate) lsp_context_enabled: bool,
    pub(crate) soft_compaction_threshold: f32,
    pub(crate) hard_compaction_threshold: f32,
    // Experiments
    pub(crate) experiments_enabled: bool,
    pub(crate) experiments_eval_model: Option<String>,
    pub(crate) experiments_schedule_enabled: bool,
    pub(crate) experiments_schedule_cron: String,
    // Security
    pub(crate) pii_filter_enabled: bool,
    pub(crate) rate_limit_enabled: bool,
    pub(crate) skill_scan_on_load: bool,
    pub(crate) skill_cross_session_rollout: bool,
    pub(crate) skill_min_sessions_before_promote: u32,
    pub(crate) skill_capability_escalation_check: bool,
    pub(crate) arise_enabled: bool,
    pub(crate) stem_enabled: bool,
    pub(crate) erl_enabled: bool,
    pub(crate) d2skill_enabled: bool,
    pub(crate) rl_routing_enabled: bool,
    pub(crate) pre_execution_verify_enabled: bool,
    pub(crate) pre_execution_verify_allowed_paths: Vec<String>,
    pub(crate) guardrail_enabled: bool,
    pub(crate) guardrail_provider: String,
    pub(crate) guardrail_model: String,
    pub(crate) guardrail_action: String,
    pub(crate) guardrail_timeout_ms: u64,
    #[cfg(feature = "classifiers")]
    pub(crate) classifiers_enabled: bool,
    #[cfg(feature = "classifiers")]
    pub(crate) pii_enabled: bool,
    pub(crate) egress_logging_enabled: bool,
    pub(crate) vigil_enabled: bool,
    pub(crate) vigil_strict_mode: bool,
    // Logging
    pub(crate) log_file: String,
    pub(crate) log_level: String,
    pub(crate) log_rotation: String,
    pub(crate) log_max_files: usize,
    // Shutdown summary
    pub(crate) shutdown_summary: bool,
    // Policy enforcer
    pub(crate) policy_enforcer_enabled: bool,
    /// Deployment bundle selected in the mode step (e.g. "desktop", "ide", "server").
    pub(crate) deployment_bundle: Option<String>,
    pub(crate) semantic_cache_enabled: bool,
    pub(crate) semantic_cache_threshold: f32,
    // Compaction probe (#2048)
    pub(crate) probe_enabled: bool,
    pub(crate) probe_provider: Option<String>,
    pub(crate) probe_threshold: f32,
    pub(crate) probe_hard_fail_threshold: f32,
    // Tool retry config
    pub(crate) retry_max_attempts: usize,
    pub(crate) retry_parameter_reformat_provider: String,
    // Session digest (#2289)
    pub(crate) digest_enabled: bool,
    // Context strategy (#2288)
    pub(crate) context_strategy: String,
    // MCP tool discovery (#2321)
    pub(crate) mcp_discovery_strategy: String,
    pub(crate) mcp_discovery_top_k: usize,
    pub(crate) mcp_discovery_provider: String,
    /// `PostgreSQL` database URL (set when user selects postgres backend in `step_memory`).
    /// Empty string means the user chose postgres but was instructed to store URL in vault.
    pub(crate) database_url: Option<String>,
    // Transactional shell (#2414)
    pub(crate) shell_transactional: bool,
    pub(crate) shell_auto_rollback: bool,
    // File read sandbox (#2525)
    pub(crate) file_deny_read: Vec<String>,
    pub(crate) file_allow_read: Vec<String>,
    // OS subprocess sandbox (#3070, #3077)
    pub(crate) sandbox_enabled: bool,
    pub(crate) sandbox_profile: String,
    pub(crate) sandbox_backend: String,
    pub(crate) sandbox_strict: bool,
    pub(crate) sandbox_allow_read: Vec<String>,
    pub(crate) sandbox_allow_write: Vec<String>,
    // Budget hint injection (#2267)
    pub(crate) budget_hint_enabled: bool,
    // SleepGate forgetting sweep (#2397)
    pub(crate) forgetting_enabled: bool,
    // Compression ratio predictor (#2460)
    pub(crate) compression_predictor_enabled: bool,
    // Time-based microcompact (#2699)
    pub(crate) microcompact_enabled: bool,
    pub(crate) microcompact_gap_threshold_minutes: u32,
    // autoDream background consolidation (#2697)
    pub(crate) autodream_enabled: bool,
    pub(crate) autodream_min_sessions: u32,
    pub(crate) autodream_min_hours: u32,
    // MagicDocs auto-maintained markdown (#2702)
    pub(crate) magic_docs_enabled: bool,
    // Profiling and distributed tracing (#2846)
    pub(crate) telemetry_enabled: bool,
    // Prometheus metrics export (#2866)
    pub(crate) prometheus_enabled: bool,
}

impl Default for WizardState {
    #[allow(clippy::too_many_lines)]
    fn default() -> Self {
        Self {
            provider: None,
            base_url: None,
            model: None,
            embedding_model: None,
            vision_model: None,
            api_key: None,
            compatible_name: None,
            sqlite_path: None,
            sessions_max_history: 0,
            sessions_title_max_chars: 0,
            qdrant_url: None,
            semantic_enabled: false,
            channel: ChannelChoice::default(),
            telegram_token: None,
            telegram_users: Vec::new(),
            discord_token: None,
            discord_app_id: None,
            slack_bot_token: None,
            slack_signing_secret: None,
            vault_backend: String::new(),
            auto_update_check: false,
            scheduler_enabled: false,
            scheduler_tick_interval_secs: 0,
            scheduler_max_tasks: 0,
            daemon_enabled: false,
            daemon_host: String::new(),
            daemon_port: 0,
            daemon_auth_token: None,
            acp_enabled: false,
            acp_agent_name: String::new(),
            acp_agent_version: String::new(),
            thinking: None,
            enable_extended_context: false,
            agents_default_permission_mode: None,
            agents_default_disallowed_tools: Vec::new(),
            agents_allow_bypass_permissions: false,
            agents_user_dir: None,
            agents_default_memory_scope: None,
            detector_mode: None,
            judge_model: None,
            feedback_provider: None,
            router_strategy: None,
            router_thompson_state_path: None,
            router_cascade_quality_threshold: None,
            router_cascade_max_escalations: None,
            router_cascade_cost_tiers: None,
            orchestration_enabled: false,
            orchestration_max_tasks: 0,
            orchestration_max_parallel: 0,
            orchestration_confirm_before_execute: false,
            orchestration_failure_strategy: String::new(),
            orchestration_planner_provider: None,
            orchestration_persistence_enabled: true,
            debug_dump_enabled: false,
            debug_dump_format: zeph_core::debug_dump::DumpFormat::Json,
            graph_memory_enabled: false,
            graph_extract_model: None,
            graph_spreading_activation_enabled: false,
            compression_guidelines_enabled: false,
            focus_enabled: false,
            focus_compression_interval: 12,
            sidequest_enabled: false,
            sidequest_interval_turns: 4,
            pruning_strategy: "reactive".into(),
            memory_tiers_enabled: false,
            memory_tiers_promotion_min_sessions: 3,
            gemini_thinking_level: None,
            server_compaction_enabled: false,
            mcpls_enabled: false,
            mcpls_workspace_roots: Vec::new(),
            mcp_remote_servers: Vec::new(),
            lsp_context_enabled: false,
            // Valid sentinel values so WizardState is usable outside run() without
            // out-of-range values; run() initialises these to the same values explicitly.
            soft_compaction_threshold: 0.60,
            hard_compaction_threshold: 0.90,
            experiments_enabled: false,
            experiments_eval_model: None,
            experiments_schedule_enabled: false,
            experiments_schedule_cron: String::new(),
            pii_filter_enabled: false,
            rate_limit_enabled: false,
            skill_scan_on_load: true,
            skill_cross_session_rollout: false,
            skill_min_sessions_before_promote: 2,
            skill_capability_escalation_check: false,
            arise_enabled: false,
            stem_enabled: false,
            erl_enabled: false,
            d2skill_enabled: false,
            rl_routing_enabled: false,
            pre_execution_verify_enabled: true,
            pre_execution_verify_allowed_paths: Vec::new(),
            guardrail_enabled: false,
            guardrail_provider: "ollama".to_owned(),
            guardrail_model: "llama-guard-3:1b".to_owned(),
            guardrail_action: "block".to_owned(),
            guardrail_timeout_ms: 500,
            #[cfg(feature = "classifiers")]
            classifiers_enabled: false,
            #[cfg(feature = "classifiers")]
            pii_enabled: false,
            egress_logging_enabled: true,
            vigil_enabled: true,
            vigil_strict_mode: false,
            log_file: String::new(),
            log_level: String::new(),
            log_rotation: String::new(),
            log_max_files: 0,
            shutdown_summary: true,
            policy_enforcer_enabled: false,
            deployment_bundle: None,
            semantic_cache_enabled: false,
            semantic_cache_threshold: 0.95,
            probe_enabled: false,
            probe_provider: None,
            probe_threshold: 0.6,
            probe_hard_fail_threshold: 0.35,
            retry_max_attempts: 2,
            retry_parameter_reformat_provider: String::new(),
            digest_enabled: false,
            context_strategy: "full_history".to_owned(),
            mcp_discovery_strategy: "none".to_owned(),
            mcp_discovery_top_k: 10,
            mcp_discovery_provider: String::new(),
            database_url: None,
            shell_transactional: false,
            shell_auto_rollback: false,
            file_deny_read: Vec::new(),
            file_allow_read: Vec::new(),
            sandbox_enabled: false,
            sandbox_profile: "workspace".to_owned(),
            sandbox_backend: "auto".to_owned(),
            sandbox_strict: true,
            sandbox_allow_read: Vec::new(),
            sandbox_allow_write: Vec::new(),
            budget_hint_enabled: true,
            forgetting_enabled: false,
            compression_predictor_enabled: false,
            microcompact_enabled: false,
            microcompact_gap_threshold_minutes: 60,
            autodream_enabled: false,
            autodream_min_sessions: 5,
            autodream_min_hours: 8,
            magic_docs_enabled: false,
            telemetry_enabled: false,
            prometheus_enabled: false,
        }
    }
}

#[derive(Default, Clone, Copy)]
pub(crate) enum ChannelChoice {
    #[default]
    Cli,
    Telegram,
    Discord,
    Slack,
}

pub fn run(output: Option<PathBuf>) -> anyhow::Result<()> {
    println!("zeph init - configuration wizard\n");

    let mut state = WizardState {
        vault_backend: "env".into(),
        semantic_enabled: true,
        auto_update_check: true,
        scheduler_tick_interval_secs: 60,
        scheduler_max_tasks: 100,
        daemon_host: "127.0.0.1".into(),
        daemon_port: 8080,
        acp_agent_name: "zeph".into(),
        acp_agent_version: env!("CARGO_PKG_VERSION").into(),
        orchestration_max_tasks: 20,
        orchestration_max_parallel: 4,
        orchestration_confirm_before_execute: true,
        orchestration_failure_strategy: "abort".into(),
        soft_compaction_threshold: 0.60,
        hard_compaction_threshold: 0.90,
        log_file: zeph_core::config::default_log_file_path(),
        log_level: "info".into(),
        log_rotation: "daily".into(),
        log_max_files: 7,
        ..WizardState::default()
    };

    step_deployment_mode(&mut state)?;
    step_vault(&mut state)?;
    step_llm(&mut state)?;
    step_memory(&mut state)?;
    step_context_compression(&mut state)?;
    step_channel(&mut state)?;
    step_update_check(&mut state)?;
    step_scheduler(&mut state)?;
    step_orchestration(&mut state)?;
    step_daemon(&mut state)?;
    step_acp(&mut state)?;
    step_mcpls(&mut state)?;
    step_mcp_remote(&mut state)?;
    step_mcp_discovery(&mut state)?;
    step_lsp_context(&mut state)?;
    step_agents(&mut state)?;
    step_router(&mut state)?;
    step_learning(&mut state)?;
    step_security(&mut state)?;
    step_sandbox(&mut state)?;
    step_debug(&mut state)?;
    step_logging(&mut state)?;
    step_experiments(&mut state)?;
    step_retry(&mut state)?;
    step_policy(&mut state)?;
    step_telemetry(&mut state)?;
    step_prometheus(&mut state)?;
    step_review_and_write(&state, output)?;

    Ok(())
}
#[allow(clippy::too_many_lines)]
fn step_channel(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 4/10: Channel ==\n");

    let use_age = state.vault_backend == "age";

    let channels = ["CLI only (default)", "Telegram", "Discord", "Slack"];
    let selection = Select::new()
        .with_prompt("Select communication channel")
        .items(channels)
        .default(0)
        .interact()?;

    match selection {
        0 => state.channel = ChannelChoice::Cli,
        1 => {
            state.channel = ChannelChoice::Telegram;
            if !use_age {
                state.telegram_token = Some(
                    Password::new()
                        .with_prompt("Telegram bot token")
                        .interact()?,
                );
            }
            let users: String = Input::new()
                .with_prompt("Allowed usernames (comma-separated)")
                .default(String::new())
                .interact_text()?;
            state.telegram_users = users
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        }
        2 => {
            state.channel = ChannelChoice::Discord;
            if !use_age {
                state.discord_token = Some(
                    Password::new()
                        .with_prompt("Discord bot token")
                        .interact()?,
                );
            }
            state.discord_app_id = Some(
                Input::new()
                    .with_prompt("Discord application ID")
                    .interact_text()?,
            );
        }
        3 => {
            state.channel = ChannelChoice::Slack;
            if !use_age {
                state.slack_bot_token =
                    Some(Password::new().with_prompt("Slack bot token").interact()?);
                state.slack_signing_secret = Some(
                    Password::new()
                        .with_prompt("Slack signing secret")
                        .interact()?,
                );
            }
        }
        _ => unreachable!(),
    }

    println!();
    Ok(())
}

fn step_deployment_mode(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Deployment Mode ==\n");
    println!("Select the primary mode you will use Zeph in.");
    println!("This determines which --features flag to pass when building from source.");
    println!("Pre-built binaries already include all features.\n");

    let modes = [
        "CLI (no extras — minimal build)",
        "Desktop (TUI dashboard + scheduler + compression guidelines)",
        "IDE (ACP integration for Zed / Helix / VS Code + LSP context)",
        "Server (HTTP gateway + A2A protocol + scheduler + OpenTelemetry)",
        "Chat (Discord + Slack bots)",
        "ML (local Candle inference + PDF + speech-to-text)",
        "Full (all optional features except hardware GPU flags)",
    ];
    let sel = Select::new()
        .with_prompt("Deployment mode")
        .items(modes)
        .default(0)
        .interact()?;

    state.deployment_bundle = match sel {
        1 => Some("desktop".into()),
        2 => Some("ide".into()),
        3 => Some("server".into()),
        4 => Some("chat".into()),
        5 => Some("ml".into()),
        6 => Some("full".into()),
        _ => None,
    };

    println!();
    Ok(())
}

fn step_vault(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 1/10: Secrets Backend ==\n");

    let backends = ["env (environment variables)", "age (encrypted file)"];
    let selection = Select::new()
        .with_prompt("Select secrets backend")
        .items(backends)
        .default(0)
        .interact()?;

    state.vault_backend = match selection {
        0 => "env".into(),
        1 => "age".into(),
        _ => unreachable!(),
    };

    println!();
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(crate) fn build_config(state: &WizardState) -> Config {
    let mut config = Config::default();
    config.agent.auto_update_check = state.auto_update_check;
    config.agent.budget_hint_enabled = state.budget_hint_enabled;
    let provider = state.provider.unwrap_or(ProviderKind::Ollama);

    // Build the providers pool.
    let providers = {
        // Single provider.
        vec![ProviderEntry {
            provider_type: provider,
            name: state.compatible_name.clone(),
            model: state.model.clone(),
            base_url: state.base_url.clone(),
            max_tokens: match provider {
                ProviderKind::Claude => Some(8096),
                ProviderKind::Gemini => Some(8192),
                _ => None,
            },
            embedding_model: state.embedding_model.clone(),
            thinking: state.thinking.clone(),
            server_compaction: state.server_compaction_enabled,
            enable_extended_context: state.enable_extended_context,
            thinking_level: state.gemini_thinking_level,
            vision_model: state.vision_model.clone().filter(|s| !s.is_empty()),
            ..ProviderEntry::default()
        }]
    };

    let routing = state
        .router_strategy
        .as_deref()
        .map_or(LlmRoutingStrategy::None, |s| match s {
            "thompson" => LlmRoutingStrategy::Thompson,
            "cascade" => LlmRoutingStrategy::Cascade,
            _ => LlmRoutingStrategy::Ema,
        });

    config.llm = LlmConfig {
        providers,
        routing,
        routes: std::collections::HashMap::new(),
        embedding_model: state
            .embedding_model
            .clone()
            .unwrap_or_else(|| "qwen3-embedding".into()),
        candle: None,
        router: None,
        stt: None,
        response_cache_enabled: false,
        response_cache_ttl_secs: 3600,
        semantic_cache_enabled: state.semantic_cache_enabled,
        semantic_cache_threshold: state.semantic_cache_threshold,
        semantic_cache_max_candidates: 10,
        router_ema_enabled: state.router_strategy.as_deref().is_some_and(|s| s == "ema"),
        router_ema_alpha: 0.1,
        router_reorder_interval: 10,
        instruction_file: None,
        summary_model: None,
        summary_provider: None,
        complexity_routing: None,
        coe: None,
    };

    // When postgres backend was chosen, sqlite_path is left at its serde default (unused).
    // When sqlite backend was chosen, database_url stays None.
    let sqlite_path = if state.database_url.is_some() {
        // Postgres selected: skip writing sqlite_path (leave serde default).
        zeph_core::config::default_sqlite_path()
    } else {
        state
            .sqlite_path
            .clone()
            .unwrap_or_else(zeph_core::config::default_sqlite_path)
    };
    config.memory = MemoryConfig {
        sqlite_path,
        qdrant_url: state
            .qdrant_url
            .clone()
            .unwrap_or_else(|| "http://localhost:6334".into()),
        semantic: SemanticConfig {
            enabled: state.semantic_enabled,
            ..SemanticConfig::default()
        },
        sessions: SessionsConfig {
            max_history: state.sessions_max_history,
            title_max_chars: state.sessions_title_max_chars,
        },
        database_url: state.database_url.clone(),
        ..config.memory
    };
    config.memory.graph.enabled = state.graph_memory_enabled;
    if let Some(ref m) = state.graph_extract_model {
        config.memory.graph.extract_model.clone_from(m);
    }
    config.memory.graph.spreading_activation.enabled = state.graph_spreading_activation_enabled;
    config.memory.compression_guidelines.enabled = state.compression_guidelines_enabled;
    config.agent.focus.enabled = state.focus_enabled;
    if state.focus_enabled {
        config.agent.focus.compression_interval = state.focus_compression_interval;
    }
    config.memory.sidequest.enabled = state.sidequest_enabled;
    if state.sidequest_enabled {
        config.memory.sidequest.interval_turns = state.sidequest_interval_turns;
    }
    config.memory.tiers.enabled = state.memory_tiers_enabled;
    if state.memory_tiers_enabled {
        config.memory.tiers.promotion_min_sessions = state.memory_tiers_promotion_min_sessions;
    }
    config.memory.compression.pruning_strategy = match state.pruning_strategy.as_str() {
        "task_aware" => PruningStrategy::TaskAware,
        "mig" => PruningStrategy::Mig,
        "subgoal" => PruningStrategy::Subgoal,
        "subgoal_mig" => PruningStrategy::SubgoalMig,
        _ => PruningStrategy::Reactive,
    };
    config.memory.soft_compaction_threshold = state.soft_compaction_threshold;
    config.memory.hard_compaction_threshold = state.hard_compaction_threshold;
    config.memory.compression.probe.enabled = state.probe_enabled;
    if let Some(ref p) = state.probe_provider {
        config.memory.compression.probe.probe_provider.clone_from(p);
    }
    if state.probe_enabled {
        config.memory.compression.probe.threshold = state.probe_threshold;
        config.memory.compression.probe.hard_fail_threshold = state.probe_hard_fail_threshold;
    }
    config.memory.shutdown_summary = state.shutdown_summary;
    config.memory.digest.enabled = state.digest_enabled;
    config.memory.context_strategy = match state.context_strategy.as_str() {
        "memory_first" => zeph_core::config::ContextStrategy::MemoryFirst,
        "adaptive" => zeph_core::config::ContextStrategy::Adaptive,
        _ => zeph_core::config::ContextStrategy::FullHistory,
    };

    match state.channel {
        ChannelChoice::Cli => {}
        ChannelChoice::Telegram => {
            config.telegram = Some(TelegramConfig {
                token: None,
                allowed_users: state.telegram_users.clone(),
                skills: ChannelSkillsConfig::default(),
            });
        }
        ChannelChoice::Discord => {
            config.discord = Some(DiscordConfig {
                token: None,
                application_id: state.discord_app_id.clone(),
                allowed_user_ids: vec![],
                allowed_role_ids: vec![],
                allowed_channel_ids: vec![],
                skills: ChannelSkillsConfig::default(),
            });
        }
        ChannelChoice::Slack => {
            config.slack = Some(SlackConfig {
                bot_token: None,
                signing_secret: None,
                webhook_host: "127.0.0.1".into(),
                port: 3000,
                allowed_user_ids: vec![],
                allowed_channel_ids: vec![],
                skills: ChannelSkillsConfig::default(),
            });
        }
    }

    config.vault = VaultConfig {
        backend: state.vault_backend.clone(),
    };

    apply_daemon_config(&mut config, state);
    apply_acp_config(&mut config, state);

    config.scheduler = zeph_core::config::SchedulerConfig {
        enabled: state.scheduler_enabled,
        tick_interval_secs: state.scheduler_tick_interval_secs,
        max_tasks: state.scheduler_max_tasks,
        tasks: Vec::new(),
    };

    config.agents.default_permission_mode = state.agents_default_permission_mode;
    config
        .agents
        .default_disallowed_tools
        .clone_from(&state.agents_default_disallowed_tools);
    config.agents.allow_bypass_permissions = state.agents_allow_bypass_permissions;
    config
        .agents
        .user_agents_dir
        .clone_from(&state.agents_user_dir);
    config.agents.default_memory_scope = state.agents_default_memory_scope;

    match state.detector_mode.as_deref() {
        Some("judge") => {
            config.skills.learning.detector_mode = zeph_core::config::DetectorMode::Judge;
            if let Some(ref model) = state.judge_model {
                config.skills.learning.judge_model.clone_from(model);
            }
        }
        Some("model") => {
            config.skills.learning.detector_mode = zeph_core::config::DetectorMode::Model;
            if let Some(ref provider) = state.feedback_provider {
                config.skills.learning.feedback_provider = ProviderName::new(provider);
            }
        }
        _ => {}
    }

    config.orchestration = OrchestrationConfig {
        enabled: state.orchestration_enabled,
        max_tasks: state.orchestration_max_tasks,
        max_parallel: state.orchestration_max_parallel,
        confirm_before_execute: state.orchestration_confirm_before_execute,
        default_failure_strategy: state.orchestration_failure_strategy.clone(),
        planner_provider: ProviderName::new(
            state
                .orchestration_planner_provider
                .clone()
                .unwrap_or_default(),
        ),
        persistence_enabled: state.orchestration_persistence_enabled,
        ..OrchestrationConfig::default()
    };

    config.debug.enabled = state.debug_dump_enabled;
    config.debug.format = state.debug_dump_format;

    config.security.pii_filter.enabled = state.pii_filter_enabled;
    config.security.rate_limit.enabled = state.rate_limit_enabled;
    config.security.pre_execution_verify.enabled = state.pre_execution_verify_enabled;
    if !state.pre_execution_verify_allowed_paths.is_empty() {
        config
            .security
            .pre_execution_verify
            .destructive_commands
            .allowed_paths
            .clone_from(&state.pre_execution_verify_allowed_paths);
    }
    config.tools.egress.enabled = state.egress_logging_enabled;
    config.security.vigil.enabled = state.vigil_enabled;
    config.security.vigil.strict_mode = state.vigil_strict_mode;
    config.tools.shell.transactional = state.shell_transactional;
    config.tools.shell.auto_rollback = state.shell_auto_rollback;
    config
        .tools
        .file
        .deny_read
        .clone_from(&state.file_deny_read);
    config
        .tools
        .file
        .allow_read
        .clone_from(&state.file_allow_read);
    // OS subprocess sandbox (#3070).
    config.tools.sandbox.enabled = state.sandbox_enabled;
    config.tools.sandbox.profile = match state.sandbox_profile.as_str() {
        "read-only" => zeph_tools::sandbox::SandboxProfile::ReadOnly,
        "network-allow-all" => zeph_tools::sandbox::SandboxProfile::NetworkAllowAll,
        "off" => zeph_tools::sandbox::SandboxProfile::Off,
        other => {
            tracing::warn!(
                "unknown sandbox_profile value {:?}; defaulting to Workspace",
                other
            );
            zeph_tools::sandbox::SandboxProfile::Workspace
        }
    };
    config
        .tools
        .sandbox
        .backend
        .clone_from(&state.sandbox_backend);
    config.tools.sandbox.strict = state.sandbox_strict;
    config.tools.sandbox.allow_read = state
        .sandbox_allow_read
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    config.tools.sandbox.allow_write = state
        .sandbox_allow_write
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    config.skills.trust.scan_on_load = state.skill_scan_on_load;
    config.skills.trust.scanner.capability_escalation_check =
        state.skill_capability_escalation_check;
    if state.skill_cross_session_rollout {
        config.skills.learning.cross_session_rollout = true;
        config.skills.learning.min_sessions_before_promote =
            state.skill_min_sessions_before_promote;
    }
    config.skills.learning.arise_enabled = state.arise_enabled;
    config.skills.learning.stem_enabled = state.stem_enabled;
    config.skills.learning.erl_enabled = state.erl_enabled;
    config.skills.learning.d2skill_enabled = state.d2skill_enabled;
    config.skills.rl_routing_enabled = state.rl_routing_enabled;
    if state.guardrail_enabled {
        config.security.guardrail.enabled = true;
        config.security.guardrail.provider = Some(state.guardrail_provider.clone());
        if !state.guardrail_model.is_empty() {
            config.security.guardrail.model = Some(state.guardrail_model.clone());
        }
        config.security.guardrail.action = match state.guardrail_action.as_str() {
            "warn" => zeph_sanitizer::guardrail::GuardrailAction::Warn,
            _ => zeph_sanitizer::guardrail::GuardrailAction::Block,
        };
        config.security.guardrail.timeout_ms = state.guardrail_timeout_ms;
    }
    {
        config.tools.policy.enabled = state.policy_enforcer_enabled;
    }

    #[cfg(feature = "classifiers")]
    {
        config.classifiers.enabled = state.classifiers_enabled;
        config.classifiers.pii_enabled = state.pii_enabled;
    }

    config.tools.retry.max_attempts = state.retry_max_attempts;
    config
        .tools
        .retry
        .parameter_reformat_provider
        .clone_from(&state.retry_parameter_reformat_provider);

    config.logging.file.clone_from(&state.log_file);
    config.logging.level.clone_from(&state.log_level);
    config.logging.rotation = match state.log_rotation.as_str() {
        "hourly" => zeph_core::config::LogRotation::Hourly,
        "never" => zeph_core::config::LogRotation::Never,
        _ => zeph_core::config::LogRotation::Daily,
    };
    config.logging.max_files = state.log_max_files;
    if state.lsp_context_enabled {
        config.lsp.enabled = true;
    }

    if state.mcpls_enabled {
        // mcpls 0.3.4 does not support --workspace-root; pass a config file instead.
        // Workspace roots and language server settings are written to .zeph/mcpls.toml
        // by write_mcpls_config() in step_review_and_write().
        config.mcp.servers.push(McpServerConfig {
            id: "mcpls".to_owned(),
            command: Some("mcpls".to_owned()),
            args: vec!["--config".to_owned(), ".zeph/mcpls.toml".to_owned()],
            env: std::collections::HashMap::new(),
            url: None,
            headers: std::collections::HashMap::new(),
            oauth: None,
            timeout: 60,
            policy: zeph_mcp::McpPolicy::default(),
            trust_level: McpTrustLevel::Trusted,
            tool_allowlist: None,
            expected_tools: Vec::new(),
            roots: Vec::new(),
            tool_metadata: std::collections::HashMap::new(),
            elicitation_enabled: None,
            env_isolation: None,
        });
    }
    for server in state.mcp_remote_servers.clone() {
        config.mcp.servers.push(server);
    }

    config.mcp.tool_discovery.strategy = match state.mcp_discovery_strategy.as_str() {
        "embedding" => zeph_core::config::ToolDiscoveryStrategyConfig::Embedding,
        "llm" => zeph_core::config::ToolDiscoveryStrategyConfig::Llm,
        _ => zeph_core::config::ToolDiscoveryStrategyConfig::None,
    };
    if state.mcp_discovery_strategy == "embedding" {
        config.mcp.tool_discovery.top_k = state.mcp_discovery_top_k;
        config.mcp.tool_discovery.embedding_provider =
            ProviderName::new(&state.mcp_discovery_provider);
    }

    if state.experiments_enabled {
        config.experiments.enabled = true;
        config
            .experiments
            .eval_model
            .clone_from(&state.experiments_eval_model);
        if state.experiments_schedule_enabled {
            config.experiments.schedule.enabled = true;
            if !state.experiments_schedule_cron.is_empty() {
                config
                    .experiments
                    .schedule
                    .cron
                    .clone_from(&state.experiments_schedule_cron);
            }
        }
    }

    config.memory.forgetting.enabled = state.forgetting_enabled;
    config.memory.compression.predictor.enabled = state.compression_predictor_enabled;
    config.memory.microcompact.enabled = state.microcompact_enabled;
    config.memory.microcompact.gap_threshold_minutes = state.microcompact_gap_threshold_minutes;
    config.memory.autodream.enabled = state.autodream_enabled;
    config.memory.autodream.min_sessions = state.autodream_min_sessions;
    config.memory.autodream.min_hours = state.autodream_min_hours;
    config.magic_docs.enabled = state.magic_docs_enabled;
    config.telemetry.enabled = state.telemetry_enabled;
    if state.prometheus_enabled {
        config.metrics.enabled = true;
        // Only enable gateway if not already enabled by the deployment bundle.
        if !config.gateway.enabled {
            config.gateway.enabled = true;
        }
    }

    config
}

fn apply_daemon_config(config: &mut Config, state: &WizardState) {
    if state.daemon_enabled {
        config.a2a.enabled = true;
        config.a2a.host.clone_from(&state.daemon_host);
        config.a2a.port = state.daemon_port;
        config.a2a.auth_token.clone_from(&state.daemon_auth_token);
    }
}

fn apply_acp_config(config: &mut Config, state: &WizardState) {
    if state.acp_enabled {
        config.acp = AcpConfig {
            enabled: true,
            agent_name: state.acp_agent_name.clone(),
            agent_version: state.acp_agent_version.clone(),
            ..AcpConfig::default()
        };
    }
}

fn step_update_check(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 5/10: Update Check ==\n");

    state.auto_update_check = Confirm::new()
        .with_prompt("Enable automatic update checks?")
        .default(true)
        .interact()?;

    state.budget_hint_enabled = Confirm::new()
        .with_prompt(
            "Inject budget hints into the system prompt so the LLM can self-regulate tool calls and cost? (budget_hint_enabled)",
        )
        .default(true)
        .interact()?;

    println!();
    Ok(())
}

fn step_scheduler(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 6/10: Scheduler ==\n");

    state.scheduler_enabled = Confirm::new()
        .with_prompt("Enable background task scheduler?")
        .default(false)
        .interact()?;

    if state.scheduler_enabled {
        state.scheduler_tick_interval_secs = Input::new()
            .with_prompt("Tick interval in seconds")
            .default(60u64)
            .interact_text()?;

        state.scheduler_max_tasks = Input::new()
            .with_prompt("Maximum scheduled tasks")
            .default(100usize)
            .interact_text()?;
    }

    println!();
    Ok(())
}
fn step_daemon(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 7/10: Daemon / A2A Server ==\n");

    state.daemon_enabled = Confirm::new()
        .with_prompt("Enable A2A daemon server?")
        .default(false)
        .interact()?;

    if state.daemon_enabled {
        state.daemon_host = Input::new()
            .with_prompt("Bind address")
            .default("127.0.0.1".into())
            .interact_text()?;

        state.daemon_port = Input::new()
            .with_prompt("Port")
            .default(8080u16)
            .interact_text()?;

        let raw: String = Password::new()
            .with_prompt("Auth token (leave empty to disable)")
            .allow_empty_password(true)
            .interact()?;
        state.daemon_auth_token = if raw.is_empty() { None } else { Some(raw) };
    }

    println!();
    Ok(())
}

fn step_acp(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 8/10: ACP Server (IDE Embedding) ==\n");

    state.acp_enabled = Confirm::new()
        .with_prompt("Enable ACP server for IDE embedding?")
        .default(false)
        .interact()?;

    if state.acp_enabled {
        state.acp_agent_name = Input::new()
            .with_prompt("Agent name")
            .default(state.acp_agent_name.clone())
            .interact_text()?;

        state.acp_agent_version = Input::new()
            .with_prompt("Agent version")
            .default(state.acp_agent_version.clone())
            .interact_text()?;
    }

    println!();
    Ok(())
}
/// Returns `true` if `mcpls` exists as an executable file on PATH.
///
/// Uses a PATH walk rather than spawning the process to avoid blocking the wizard
/// on a broken binary that enters an infinite loop.
/// Writes `.zeph/mcpls.toml` next to `config_path` so that `mcpls --config .zeph/mcpls.toml`
/// starts with the configured workspace roots and language server definitions.
///
/// # Errors
///
/// Returns an error if the directory cannot be created or the file cannot be written.
#[allow(clippy::too_many_lines)]
fn step_lsp_context(state: &mut WizardState) -> anyhow::Result<()> {
    if !state.mcpls_enabled {
        // LSP context injection requires mcpls to be configured.
        state.lsp_context_enabled = false;
        return Ok(());
    }

    println!("== LSP Context Injection ==\n");
    println!("Automatically injects diagnostics and hover info into agent context.");

    state.lsp_context_enabled = dialoguer::Confirm::new()
        .with_prompt("Enable automatic LSP context injection (diagnostics after writes)?")
        .default(true)
        .interact()?;

    println!();
    Ok(())
}
#[allow(clippy::too_many_lines)]
fn step_debug(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Debug ==\n");
    state.debug_dump_enabled = Confirm::new()
        .with_prompt(
            "Enable debug dump on startup? (saves LLM requests/responses and tool output to files)",
        )
        .default(false)
        .interact()?;

    if state.debug_dump_enabled {
        let format_options = &[
            "json (internal zeph-llm format)",
            "raw (actual API payload)",
            "trace (OpenTelemetry OTLP spans)",
        ];
        let idx = Select::new()
            .with_prompt("Debug dump format")
            .items(format_options)
            .default(0)
            .interact()?;
        state.debug_dump_format = match idx {
            1 => zeph_core::debug_dump::DumpFormat::Raw,
            2 => zeph_core::debug_dump::DumpFormat::Trace,
            _ => zeph_core::debug_dump::DumpFormat::Json,
        };
    }

    println!();
    Ok(())
}

fn step_logging(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Logging ==\n");
    println!("File logging writes structured logs to disk, separate from stderr output.");
    println!("Leave the path empty to disable file logging.\n");

    let log_file: String = Input::new()
        .with_prompt("Log file path (empty to disable)")
        .default(state.log_file.clone())
        .allow_empty(true)
        .interact_text()?;
    state.log_file = log_file;

    if !state.log_file.is_empty() {
        const VALID_LEVELS: &[&str] = &["error", "warn", "info", "debug", "trace", "off"];
        let log_level: String = Input::new()
            .with_prompt(format!("File log level [{}]", VALID_LEVELS.join("|")))
            .default(state.log_level.clone())
            .validate_with(|input: &String| {
                if VALID_LEVELS.contains(&input.to_lowercase().as_str()) {
                    Ok(())
                } else {
                    Err(format!(
                        "invalid level '{input}'; choose one of: {}",
                        VALID_LEVELS.join(", ")
                    ))
                }
            })
            .interact_text()?;
        state.log_level = log_level;

        let rotation_idx = Select::new()
            .with_prompt("Log rotation")
            .items(["daily", "hourly", "never"])
            .default(0)
            .interact()?;
        state.log_rotation = ["daily", "hourly", "never"][rotation_idx].into();

        if state.log_rotation != "never" {
            let max_files: String = Input::new()
                .with_prompt("Max rotated files to keep")
                .default(state.log_max_files.to_string())
                .interact_text()?;
            state.log_max_files = max_files.parse().unwrap_or(7);
        }
    }
    println!();
    Ok(())
}

fn step_experiments(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Experiments ==\n");
    println!("Autonomous self-experimentation: the agent varies its own parameters,");
    println!("evaluates via LLM-as-judge, and keeps improvements.\n");

    state.experiments_enabled = Confirm::new()
        .with_prompt("Enable autonomous experiments?")
        .default(false)
        .interact()?;

    if state.experiments_enabled {
        let model: String = Input::new()
            .with_prompt("Judge model for evaluation")
            .default("claude-sonnet-4-6-20251101".into())
            .interact_text()?;
        state.experiments_eval_model = if model.is_empty() { None } else { Some(model) };

        state.experiments_schedule_enabled = Confirm::new()
            .with_prompt("Schedule automatic experiment runs?")
            .default(false)
            .interact()?;

        if state.experiments_schedule_enabled {
            state.experiments_schedule_cron = Input::new()
                .with_prompt("Cron schedule")
                .default("0 3 * * *".into())
                .interact_text()?;
        }
    }

    state.microcompact_enabled = Confirm::new()
        .with_prompt(
            "Enable time-based microcompact? (strips stale low-value tool outputs after idle gap)",
        )
        .default(false)
        .interact()?;

    if state.microcompact_enabled {
        state.microcompact_gap_threshold_minutes = Input::new()
            .with_prompt("Idle gap in minutes before stale tool outputs are cleared")
            .default(60u32)
            .interact_text()?;
    }

    state.autodream_enabled = Confirm::new()
        .with_prompt("Enable autoDream? (background memory consolidation after N sessions)")
        .default(false)
        .interact()?;

    if state.autodream_enabled {
        state.autodream_min_sessions = Input::new()
            .with_prompt("Minimum completed sessions before consolidation")
            .default(5u32)
            .interact_text()?;
        state.autodream_min_hours = Input::new()
            .with_prompt("Minimum hours since last consolidation")
            .default(8u32)
            .interact_text()?;
    }

    state.magic_docs_enabled = Confirm::new()
        .with_prompt(
            "Enable MagicDocs? (auto-updates markdown files marked with '# MAGIC DOC:' header)",
        )
        .default(false)
        .interact()?;

    println!();
    Ok(())
}

fn step_retry(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Tool Retry Configuration ==\n");

    state.retry_max_attempts = Input::new()
        .with_prompt("Maximum retry attempts for transient tool errors (0 to disable)")
        .default(2_usize)
        .interact()?;

    let provider: String = Input::new()
        .with_prompt(
            "Provider name for LLM parameter reformatting on invalid-params errors \
             (leave empty to disable)",
        )
        .default(String::new())
        .interact_text()?;
    state.retry_parameter_reformat_provider = provider;

    println!();
    Ok(())
}
fn step_telemetry(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Profiling & Tracing ==\n");
    println!("Requires the binary to be compiled with --features profiling.");
    println!("When disabled (default), all instrumentation is compiled out — zero overhead.\n");

    state.telemetry_enabled = Confirm::new()
        .with_prompt("Enable profiling/tracing telemetry?")
        .default(false)
        .interact()?;

    println!();
    Ok(())
}

fn step_prometheus(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Prometheus Metrics Export ==\n");
    println!("Requires the binary to be compiled with --features prometheus.");
    println!("Exposes a /metrics endpoint on the HTTP gateway for Prometheus scraping.");
    println!("Enabling this will also enable [gateway] if it is not already set.\n");

    state.prometheus_enabled = Confirm::new()
        .with_prompt("Enable Prometheus metrics export?")
        .default(false)
        .interact()?;

    println!();
    Ok(())
}

fn step_review_and_write(state: &WizardState, output: Option<PathBuf>) -> anyhow::Result<()> {
    println!("== Step 10/10: Review & Write ==\n");

    let config = build_config(state);
    let toml_str = toml::to_string_pretty(&config)?;

    println!("--- Generated config ---");
    println!("{toml_str}");
    println!("------------------------\n");

    let default_path = PathBuf::from("config.toml");
    let path = output.unwrap_or_else(|| {
        Input::new()
            .with_prompt("Write config to")
            .default(default_path.display().to_string())
            .interact_text()
            .map(PathBuf::from)
            .unwrap_or(default_path)
    });

    if path.exists() {
        let overwrite = Confirm::new()
            .with_prompt(format!("{} already exists. Overwrite?", path.display()))
            .default(false)
            .interact()?;
        if !overwrite {
            println!("Aborted.");
            return Ok(());
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    zeph_common::fs_secure::atomic_write_private(&path, toml_str.as_bytes())?;
    println!("Config written to {}", path.display());

    if state.mcpls_enabled {
        write_mcpls_config(state, &path)?;
    }

    print_secrets_instructions(state);
    print_next_steps(state, &path);

    Ok(())
}

fn api_key_env_var(kind: ProviderKind, name: Option<&str>) -> Option<String> {
    match kind {
        ProviderKind::Claude => Some("ZEPH_CLAUDE_API_KEY".to_owned()),
        ProviderKind::OpenAi => Some("ZEPH_OPENAI_API_KEY".to_owned()),
        ProviderKind::Gemini => Some("ZEPH_GEMINI_API_KEY".to_owned()),
        ProviderKind::Compatible => {
            let n = name.unwrap_or("custom").to_uppercase();
            Some(format!("ZEPH_COMPATIBLE_{n}_API_KEY"))
        }
        _ => None,
    }
}

fn collect_provider_secret(
    secrets: &mut Vec<String>,
    kind: Option<ProviderKind>,
    api_key: Option<&String>,
    name: Option<&str>,
    use_age: bool,
) {
    if let Some(k) = kind
        && let Some(var) = api_key_env_var(k, name)
        && !secrets.contains(&var)
    {
        let include = if use_age {
            true
        } else {
            api_key.is_some_and(|key| !key.is_empty())
        };
        if include {
            secrets.push(var);
        }
    }
}

fn print_secrets_instructions(state: &WizardState) {
    let use_age = state.vault_backend == "age";
    let mut secrets: Vec<String> = Vec::new();

    collect_provider_secret(
        &mut secrets,
        state.provider,
        state.api_key.as_ref(),
        state.compatible_name.as_deref(),
        use_age,
    );

    let include_telegram = use_age && matches!(state.channel, ChannelChoice::Telegram)
        || state.telegram_token.is_some();
    if include_telegram {
        secrets.push("ZEPH_TELEGRAM_TOKEN".into());
    }

    let include_discord =
        use_age && matches!(state.channel, ChannelChoice::Discord) || state.discord_token.is_some();
    if include_discord {
        secrets.push("ZEPH_DISCORD_TOKEN".into());
    }

    let include_slack =
        use_age && matches!(state.channel, ChannelChoice::Slack) || state.slack_bot_token.is_some();
    if include_slack {
        secrets.push("ZEPH_SLACK_BOT_TOKEN".into());
    }

    let include_slack_secret = use_age && matches!(state.channel, ChannelChoice::Slack)
        || state.slack_signing_secret.is_some();
    if include_slack_secret && !secrets.contains(&"ZEPH_SLACK_SIGNING_SECRET".to_owned()) {
        secrets.push("ZEPH_SLACK_SIGNING_SECRET".into());
    }

    if secrets.is_empty() {
        return;
    }

    if use_age {
        println!("\nFirst run `zeph vault init` if you haven't already.");
        println!("Then store secrets:");
        for var in &secrets {
            println!("  zeph vault set {var} <value>"); // lgtm[rust/cleartext-logging]
        }
    } else {
        println!("\nAdd the following to your shell profile:");
        for var in &secrets {
            println!("  export {var}=\"<your-secret>\"");
        }
    }
}

fn print_next_steps(state: &WizardState, path: &std::path::Path) {
    println!("\nNext steps:");
    if state.vault_backend == "age" {
        println!("  1. Store secrets (see above)");
    } else {
        println!("  1. Set required environment variables (see above)");
    }
    println!("  2. Run: zeph --config {}", path.display());
    println!("  3. Or with TUI: zeph --tui --config {}", path.display());
    println!();
    if let Some(bundle) = &state.deployment_bundle {
        println!(
            "Building from source? Use the `{bundle}` bundle:\n  cargo build --release --features {bundle}"
        );
        println!();
    }
    println!("Tip: run `zeph migrate-config --diff` later to check for new config options.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_provider_state() -> WizardState {
        WizardState {
            provider: Some(ProviderKind::Claude),
            model: Some("claude-sonnet-4-5-20250929".into()),
            embedding_model: Some("qwen3-embedding".into()),
            api_key: Some("key-abc".into()),
            vault_backend: "env".into(),
            semantic_enabled: true,
            ..WizardState::default()
        }
    }

    #[test]
    fn build_config_single_provider_creates_one_entry() {
        let state = single_provider_state();
        let config = build_config(&state);
        assert_eq!(config.llm.providers.len(), 1);
        assert_eq!(config.llm.providers[0].provider_type, ProviderKind::Claude);
        assert_eq!(
            config.llm.providers[0].model.as_deref(),
            Some("claude-sonnet-4-5-20250929")
        );
    }

    #[test]
    fn build_config_single_provider_has_one_entry() {
        let state = WizardState {
            provider: Some(ProviderKind::Ollama),
            model: Some("qwen3:8b".into()),
            embedding_model: Some("qwen3-embedding".into()),
            base_url: Some("http://localhost:11434".into()),
            vault_backend: "env".into(),
            semantic_enabled: false,
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(config.llm.providers.len(), 1);
        assert_eq!(config.llm.providers[0].provider_type, ProviderKind::Ollama);
    }

    #[test]
    fn api_key_env_var_returns_correct_vars() {
        assert_eq!(
            api_key_env_var(ProviderKind::Claude, None),
            Some("ZEPH_CLAUDE_API_KEY".to_owned())
        );
        assert_eq!(
            api_key_env_var(ProviderKind::OpenAi, None),
            Some("ZEPH_OPENAI_API_KEY".to_owned())
        );
        assert_eq!(
            api_key_env_var(ProviderKind::Compatible, Some("myprovider")),
            Some("ZEPH_COMPATIBLE_MYPROVIDER_API_KEY".to_owned())
        );
        assert_eq!(api_key_env_var(ProviderKind::Ollama, None), None);
    }

    #[test]
    fn collect_provider_secret_skips_empty_key() {
        let mut secrets: Vec<String> = Vec::new();
        let empty = String::new();
        collect_provider_secret(
            &mut secrets,
            Some(ProviderKind::Claude),
            Some(&empty),
            None,
            false,
        );
        assert!(secrets.is_empty(), "empty key must not add any secret");
    }

    #[test]
    fn collect_provider_secret_deduplicates() {
        let mut secrets: Vec<String> = Vec::new();
        let key = "sk-test".to_owned();
        collect_provider_secret(
            &mut secrets,
            Some(ProviderKind::Claude),
            Some(&key),
            None,
            false,
        );
        collect_provider_secret(
            &mut secrets,
            Some(ProviderKind::Claude),
            Some(&key),
            None,
            false,
        );
        assert_eq!(
            secrets.len(),
            1,
            "duplicate provider should appear only once"
        );
        assert_eq!(secrets[0], "ZEPH_CLAUDE_API_KEY");
    }

    #[test]
    fn build_config_graph_memory_enabled() {
        let state = WizardState {
            graph_memory_enabled: true,
            graph_extract_model: Some("llama3".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.memory.graph.enabled);
        assert_eq!(config.memory.graph.extract_model, "llama3");
    }

    #[test]
    fn build_config_graph_memory_disabled() {
        let state = WizardState {
            graph_memory_enabled: false,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(!config.memory.graph.enabled);
    }

    #[test]
    fn build_config_compression_guidelines_enabled() {
        let state = WizardState {
            compression_guidelines_enabled: true,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.memory.compression_guidelines.enabled);
    }

    #[test]
    fn build_config_compression_guidelines_disabled() {
        let state = WizardState {
            compression_guidelines_enabled: false,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(!config.memory.compression_guidelines.enabled);
    }

    #[test]
    fn build_config_mcpls_enabled_produces_mcp_server() {
        let state = WizardState {
            mcpls_enabled: true,
            mcpls_workspace_roots: vec!["./crate-a".into(), "./crate-b".into()],
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(config.mcp.servers.len(), 1);
        let server = &config.mcp.servers[0];
        assert_eq!(server.id, "mcpls");
        assert_eq!(server.command.as_deref(), Some("mcpls"));
        assert_eq!(server.args, vec!["--config", ".zeph/mcpls.toml"]);
        assert_eq!(server.timeout, 60);
        // mcpls uses command+args, not an HTTP URL.
        assert!(server.url.is_none());
        // No env vars are injected for mcpls.
        assert!(server.env.is_empty());
    }

    #[test]
    fn build_config_mcpls_enabled_defaults_root_to_dot() {
        let state = WizardState {
            mcpls_enabled: true,
            mcpls_workspace_roots: vec![],
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(config.mcp.servers.len(), 1);
        let server = &config.mcp.servers[0];
        assert_eq!(server.args, vec!["--config", ".zeph/mcpls.toml"]);
    }

    #[test]
    fn build_config_mcpls_disabled_produces_no_mcp_server() {
        let state = WizardState {
            mcpls_enabled: false,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.mcp.servers.is_empty());
    }

    #[test]
    fn build_config_experiments_enabled() {
        let state = WizardState {
            experiments_enabled: true,
            experiments_eval_model: Some("claude-sonnet-4-20250514".into()),
            experiments_schedule_enabled: true,
            experiments_schedule_cron: "0 4 * * *".into(),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.experiments.enabled);
        assert_eq!(
            config.experiments.eval_model.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert!(config.experiments.schedule.enabled);
        assert_eq!(config.experiments.schedule.cron, "0 4 * * *");
    }

    #[test]
    fn build_config_experiments_disabled_by_default() {
        let state = WizardState {
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(!config.experiments.enabled);
    }

    // --- build_config logging mapping ---

    #[test]
    fn build_config_logging_defaults() {
        // WizardState::default() derives Default so string fields are empty.
        // The wizard initialises them to sensible values at runtime; here we test
        // that build_config maps state fields verbatim into config.logging.
        let state = WizardState {
            log_file: zeph_core::config::default_log_file_path(),
            log_level: "info".into(),
            log_rotation: "daily".into(),
            log_max_files: 7,
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.logging.file,
            zeph_core::config::default_log_file_path(),
            "default log file path"
        );
        assert_eq!(config.logging.level, "info");
        assert_eq!(
            config.logging.rotation,
            zeph_core::config::LogRotation::Daily
        );
        assert_eq!(config.logging.max_files, 7);
    }

    #[test]
    fn build_config_logging_custom_values() {
        let state = WizardState {
            log_file: "/tmp/custom.log".into(),
            log_level: "debug".into(),
            log_rotation: "hourly".into(),
            log_max_files: 14,
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(config.logging.file, "/tmp/custom.log");
        assert_eq!(config.logging.level, "debug");
        assert_eq!(
            config.logging.rotation,
            zeph_core::config::LogRotation::Hourly
        );
        assert_eq!(config.logging.max_files, 14);
    }

    #[test]
    fn build_config_logging_disabled_empty_file() {
        let state = WizardState {
            log_file: String::new(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(
            config.logging.file.is_empty(),
            "empty log_file should disable file logging"
        );
    }

    #[test]
    fn build_config_logging_rotation_never() {
        let state = WizardState {
            log_rotation: "never".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.logging.rotation,
            zeph_core::config::LogRotation::Never
        );
    }

    #[test]
    fn build_config_hard_compaction_threshold_custom() {
        let state = WizardState {
            soft_compaction_threshold: 0.60,
            hard_compaction_threshold: 0.85,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!((config.memory.soft_compaction_threshold - 0.60).abs() < f32::EPSILON);
        assert!((config.memory.hard_compaction_threshold - 0.85).abs() < f32::EPSILON);
    }

    #[test]
    fn build_config_hard_compaction_threshold_default() {
        let state = WizardState {
            soft_compaction_threshold: 0.70,
            hard_compaction_threshold: 0.90,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!((config.memory.soft_compaction_threshold - 0.70).abs() < f32::EPSILON);
        assert!((config.memory.hard_compaction_threshold - 0.90).abs() < f32::EPSILON);
    }

    // Documents that build_config() is a dumb mapper: cross-field validation (hard > soft)
    // lives in Config::validate(), not here.
    #[test]
    fn build_config_hard_below_soft_maps_verbatim() {
        let state = WizardState {
            soft_compaction_threshold: 0.80,
            hard_compaction_threshold: 0.60,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!((config.memory.soft_compaction_threshold - 0.80).abs() < f32::EPSILON);
        assert!((config.memory.hard_compaction_threshold - 0.60).abs() < f32::EPSILON);
    }

    // Documents that boundary exclusion (hard < 1.0) lives in the wizard validator,
    // not in build_config().
    #[test]
    fn build_config_hard_at_boundary() {
        let state = WizardState {
            soft_compaction_threshold: 0.70,
            hard_compaction_threshold: 1.0,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!((config.memory.soft_compaction_threshold - 0.70).abs() < f32::EPSILON);
        assert!((config.memory.hard_compaction_threshold - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn build_config_pre_execution_verify_enabled_default() {
        let state = WizardState {
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.security.pre_execution_verify.enabled);
    }

    #[test]
    fn build_config_pre_execution_verify_disabled() {
        let state = WizardState {
            pre_execution_verify_enabled: false,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(!config.security.pre_execution_verify.enabled);
    }

    #[test]
    fn build_config_pre_execution_verify_allowed_paths() {
        let state = WizardState {
            pre_execution_verify_enabled: true,
            pre_execution_verify_allowed_paths: vec!["/tmp".into(), "/home/user".into()],
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config
                .security
                .pre_execution_verify
                .destructive_commands
                .allowed_paths,
            vec!["/tmp", "/home/user"]
        );
    }

    #[test]
    fn build_config_pre_execution_verify_empty_paths() {
        let state = WizardState {
            pre_execution_verify_enabled: true,
            pre_execution_verify_allowed_paths: vec![],
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(
            config
                .security
                .pre_execution_verify
                .destructive_commands
                .allowed_paths
                .is_empty()
        );
    }

    #[test]
    fn build_config_focus_enabled() {
        let state = WizardState {
            focus_enabled: true,
            focus_compression_interval: 7,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.agent.focus.enabled);
        assert_eq!(config.agent.focus.compression_interval, 7);
    }

    #[test]
    fn build_config_focus_disabled_does_not_set_interval() {
        let state = WizardState {
            focus_enabled: false,
            focus_compression_interval: 7,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(!config.agent.focus.enabled);
    }

    #[test]
    fn build_config_sidequest_enabled() {
        let state = WizardState {
            sidequest_enabled: true,
            sidequest_interval_turns: 3,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.memory.sidequest.enabled);
        assert_eq!(config.memory.sidequest.interval_turns, 3);
    }

    #[test]
    fn build_config_pruning_strategy_task_aware() {
        let state = WizardState {
            pruning_strategy: "task_aware".into(),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.memory.compression.pruning_strategy,
            PruningStrategy::TaskAware
        );
    }

    #[test]
    fn build_config_pruning_strategy_mig() {
        let state = WizardState {
            pruning_strategy: "mig".into(),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.memory.compression.pruning_strategy,
            PruningStrategy::Mig
        );
    }

    #[test]
    fn build_config_pruning_strategy_task_aware_mig_falls_back_to_reactive() {
        // task_aware_mig is no longer a valid strategy; build_config treats unknown values as reactive.
        let state = WizardState {
            pruning_strategy: "task_aware_mig".into(),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.memory.compression.pruning_strategy,
            PruningStrategy::Reactive
        );
    }

    #[test]
    fn build_config_pruning_strategy_defaults_to_reactive() {
        let state = WizardState {
            pruning_strategy: "reactive".into(),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.memory.compression.pruning_strategy,
            PruningStrategy::Reactive
        );
    }

    #[test]
    fn build_config_probe_disabled_by_default() {
        let state = WizardState {
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(!config.memory.compression.probe.enabled);
    }

    #[test]
    fn build_config_probe_enabled() {
        let state = WizardState {
            vault_backend: "env".into(),
            probe_enabled: true,
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.memory.compression.probe.enabled);
    }

    #[test]
    fn build_config_probe_provider_set() {
        let state = WizardState {
            vault_backend: "env".into(),
            probe_enabled: true,
            probe_provider: Some("fast".into()),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(config.memory.compression.probe.probe_provider, "fast");
    }

    #[test]
    fn build_config_probe_provider_none_leaves_default() {
        let state = WizardState {
            vault_backend: "env".into(),
            probe_enabled: true,
            probe_provider: None,
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(config.memory.compression.probe.probe_provider, "");
    }

    #[test]
    fn build_config_probe_thresholds_propagate_when_enabled() {
        let state = WizardState {
            vault_backend: "env".into(),
            probe_enabled: true,
            probe_threshold: 0.75,
            probe_hard_fail_threshold: 0.25,
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!((config.memory.compression.probe.threshold - 0.75).abs() < f32::EPSILON);
        assert!((config.memory.compression.probe.hard_fail_threshold - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn build_config_probe_thresholds_stay_at_defaults_when_disabled() {
        let default_threshold = zeph_memory::CompactionProbeConfig::default().threshold;
        let default_hard_fail = zeph_memory::CompactionProbeConfig::default().hard_fail_threshold;
        let state = WizardState {
            vault_backend: "env".into(),
            probe_enabled: false,
            probe_threshold: 0.99,
            probe_hard_fail_threshold: 0.01,
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(
            (config.memory.compression.probe.threshold - default_threshold).abs() < f32::EPSILON
        );
        assert!(
            (config.memory.compression.probe.hard_fail_threshold - default_hard_fail).abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn build_config_postgres_backend_sets_database_url() {
        let state = WizardState {
            database_url: Some("postgres://localhost:5432/zeph".to_owned()),
            provider: Some(ProviderKind::Ollama),
            model: Some("qwen3:8b".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.memory.database_url.as_deref(),
            Some("postgres://localhost:5432/zeph"),
        );
        assert_eq!(
            config.memory.sqlite_path,
            zeph_core::config::default_sqlite_path(),
        );
    }

    #[test]
    fn build_config_sqlite_backend_leaves_database_url_none() {
        let state = WizardState {
            database_url: None,
            provider: Some(ProviderKind::Ollama),
            model: Some("qwen3:8b".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.memory.database_url.is_none());
        assert_eq!(
            config.memory.sqlite_path,
            zeph_core::config::default_sqlite_path(),
        );
    }

    #[test]
    fn build_config_file_deny_allow_mapped() {
        let state = WizardState {
            file_deny_read: vec!["/etc/shadow".into(), "/root/*".into()],
            file_allow_read: vec!["/etc/hostname".into()],
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(config.tools.file.deny_read, vec!["/etc/shadow", "/root/*"]);
        assert_eq!(config.tools.file.allow_read, vec!["/etc/hostname"]);
    }

    #[test]
    fn build_config_file_empty_by_default() {
        let state = WizardState {
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.tools.file.deny_read.is_empty());
        assert!(config.tools.file.allow_read.is_empty());
    }

    #[test]
    fn build_config_sandbox_disabled_by_default() {
        let state = single_provider_state();
        let config = build_config(&state);
        assert!(!config.tools.sandbox.enabled);
        assert!(config.tools.sandbox.strict);
    }

    #[test]
    fn build_config_sandbox_enabled_workspace() {
        let state = WizardState {
            sandbox_enabled: true,
            sandbox_profile: "workspace".into(),
            sandbox_backend: "auto".into(),
            sandbox_strict: true,
            sandbox_allow_read: vec!["/tmp/read".into()],
            sandbox_allow_write: vec!["/tmp/write".into()],
            ..single_provider_state()
        };
        let config = build_config(&state);
        assert!(config.tools.sandbox.enabled);
        assert_eq!(
            config.tools.sandbox.profile,
            zeph_tools::sandbox::SandboxProfile::Workspace
        );
        assert_eq!(config.tools.sandbox.backend, "auto");
        assert_eq!(config.tools.sandbox.allow_read.len(), 1);
        assert_eq!(config.tools.sandbox.allow_write.len(), 1);
    }

    #[test]
    fn build_config_sandbox_profile_variants() {
        for (input, expected) in [
            ("read-only", zeph_tools::sandbox::SandboxProfile::ReadOnly),
            (
                "network-allow-all",
                zeph_tools::sandbox::SandboxProfile::NetworkAllowAll,
            ),
            ("off", zeph_tools::sandbox::SandboxProfile::Off),
            ("workspace", zeph_tools::sandbox::SandboxProfile::Workspace),
        ] {
            let state = WizardState {
                sandbox_enabled: true,
                sandbox_profile: input.into(),
                ..single_provider_state()
            };
            let config = build_config(&state);
            assert_eq!(config.tools.sandbox.profile, expected, "input={input}");
        }
    }
}
