// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use dialoguer::{Confirm, Input, Password, Select};
use zeph_core::config::{
    AcpConfig, Config, DiscordConfig, LlmConfig, LlmRoutingStrategy, McpOAuthConfig,
    McpServerConfig, McpTrustLevel, MemoryConfig, OAuthTokenStorage, OrchestrationConfig,
    ProviderEntry, ProviderKind, PruningStrategy, SemanticConfig, SessionsConfig, SlackConfig,
    TelegramConfig, VaultConfig,
};
use zeph_core::subagent::def::{MemoryScope, PermissionMode};
use zeph_llm::{GeminiThinkingLevel, ThinkingConfig, ThinkingEffort};

#[cfg_attr(test, derive(Clone))]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct WizardState {
    pub(crate) provider: Option<ProviderKind>,
    /// True when the wizard configured multiple providers (primary + fallback pool).
    pub(crate) pool_mode: bool,
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
    // Orchestrator sub-provider fields
    pub(crate) orchestrator_primary_provider: Option<ProviderKind>,
    pub(crate) orchestrator_primary_model: Option<String>,
    pub(crate) orchestrator_primary_base_url: Option<String>,
    pub(crate) orchestrator_primary_api_key: Option<String>,
    pub(crate) orchestrator_primary_compatible_name: Option<String>,
    pub(crate) orchestrator_fallback_provider: Option<ProviderKind>,
    pub(crate) orchestrator_fallback_model: Option<String>,
    pub(crate) orchestrator_fallback_base_url: Option<String>,
    pub(crate) orchestrator_fallback_api_key: Option<String>,
    pub(crate) orchestrator_fallback_compatible_name: Option<String>,
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
    pub(crate) pre_execution_verify_enabled: bool,
    pub(crate) pre_execution_verify_allowed_paths: Vec<String>,
    #[cfg(feature = "guardrail")]
    pub(crate) guardrail_enabled: bool,
    #[cfg(feature = "guardrail")]
    pub(crate) guardrail_provider: String,
    #[cfg(feature = "guardrail")]
    pub(crate) guardrail_model: String,
    #[cfg(feature = "guardrail")]
    pub(crate) guardrail_action: String,
    #[cfg(feature = "guardrail")]
    pub(crate) guardrail_timeout_ms: u64,
    #[cfg(feature = "classifiers")]
    pub(crate) classifiers_enabled: bool,
    #[cfg(feature = "classifiers")]
    pub(crate) pii_enabled: bool,
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
}

impl Default for WizardState {
    #[allow(clippy::too_many_lines)]
    fn default() -> Self {
        Self {
            provider: None,
            pool_mode: false,
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
            orchestrator_primary_provider: None,
            orchestrator_primary_model: None,
            orchestrator_primary_base_url: None,
            orchestrator_primary_api_key: None,
            orchestrator_primary_compatible_name: None,
            orchestrator_fallback_provider: None,
            orchestrator_fallback_model: None,
            orchestrator_fallback_base_url: None,
            orchestrator_fallback_api_key: None,
            orchestrator_fallback_compatible_name: None,
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
            pre_execution_verify_enabled: true,
            pre_execution_verify_allowed_paths: Vec::new(),
            #[cfg(feature = "guardrail")]
            guardrail_enabled: false,
            #[cfg(feature = "guardrail")]
            guardrail_provider: "ollama".to_owned(),
            #[cfg(feature = "guardrail")]
            guardrail_model: "llama-guard-3:1b".to_owned(),
            #[cfg(feature = "guardrail")]
            guardrail_action: "block".to_owned(),
            #[cfg(feature = "guardrail")]
            guardrail_timeout_ms: 500,
            #[cfg(feature = "classifiers")]
            classifiers_enabled: false,
            #[cfg(feature = "classifiers")]
            pii_enabled: false,
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
    step_lsp_context(&mut state)?;
    step_agents(&mut state)?;
    step_router(&mut state)?;
    step_learning(&mut state)?;
    step_security(&mut state)?;
    step_debug(&mut state)?;
    step_logging(&mut state)?;
    step_experiments(&mut state)?;
    step_retry(&mut state)?;
    step_policy(&mut state)?;
    step_review_and_write(&state, output)?;

    Ok(())
}

/// `(kind, base_url, model, api_key, compatible_name)` returned by `prompt_provider_config`.
type ProviderConfig = (
    ProviderKind,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

/// Prompts for a sub-provider configuration.
/// `label` is shown to the user (e.g. "Primary" or "Fallback").
/// Returns `(kind, base_url, model, api_key, compatible_name)`.
fn prompt_provider_config(label: &str) -> anyhow::Result<ProviderConfig> {
    let sub_providers = [
        "Ollama (local)",
        "Claude (API)",
        "OpenAI (API)",
        "Compatible (custom)",
    ];
    let sel = Select::new()
        .with_prompt(format!("{label} provider"))
        .items(sub_providers)
        .default(0)
        .interact()?;

    match sel {
        0 => {
            let base_url = Input::new()
                .with_prompt("Ollama base URL")
                .default("http://localhost:11434".into())
                .interact_text()?;
            let model = Input::new()
                .with_prompt("Model name")
                .default("qwen3:8b".into())
                .interact_text()?;
            Ok((ProviderKind::Ollama, Some(base_url), model, None, None))
        }
        1 => {
            let raw = Password::new().with_prompt("Claude API key").interact()?;
            let api_key = if raw.is_empty() { None } else { Some(raw) };
            let model = Input::new()
                .with_prompt("Model name")
                .default("claude-sonnet-4-5-20250929".into())
                .interact_text()?;
            Ok((ProviderKind::Claude, None, model, api_key, None))
        }
        2 => {
            let raw = Password::new().with_prompt("OpenAI API key").interact()?;
            let api_key = if raw.is_empty() { None } else { Some(raw) };
            let base_url = Input::new()
                .with_prompt("Base URL")
                .default("https://api.openai.com/v1".into())
                .interact_text()?;
            let model = Input::new()
                .with_prompt("Model name")
                .default("gpt-4o".into())
                .interact_text()?;
            Ok((ProviderKind::OpenAi, Some(base_url), model, api_key, None))
        }
        3 => {
            let compatible_name: String =
                Input::new().with_prompt("Provider name").interact_text()?;
            let base_url = Input::new().with_prompt("Base URL").interact_text()?;
            let model = Input::new().with_prompt("Model name").interact_text()?;
            let raw = Password::new()
                .with_prompt("API key (leave empty if none)")
                .allow_empty_password(true)
                .interact()?;
            let api_key = if raw.is_empty() { None } else { Some(raw) };
            Ok((
                ProviderKind::Compatible,
                Some(base_url),
                model,
                api_key,
                Some(compatible_name),
            ))
        }
        _ => unreachable!(),
    }
}

fn step_llm(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 2/10: LLM Provider ==\n");

    let use_age = state.vault_backend == "age";

    step_llm_provider(state, use_age)?;

    state.embedding_model = Some(
        Input::new()
            .with_prompt("Embedding model")
            .default("qwen3-embedding".into())
            .interact_text()?,
    );

    if state.provider == Some(ProviderKind::Ollama) {
        let use_vision = Confirm::new()
            .with_prompt("Use a separate model for vision (image input)?")
            .default(false)
            .interact()?;
        if use_vision {
            state.vision_model = Some(
                Input::new()
                    .with_prompt("Vision model name (e.g. llava:13b)")
                    .interact_text()?,
            );
        }
    }

    println!();
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn step_llm_provider(state: &mut WizardState, use_age: bool) -> anyhow::Result<()> {
    let providers = [
        "Ollama (local)",
        "Claude (API)",
        "OpenAI (API)",
        "Gemini (API)",
        "Orchestrator (multi-model)",
        "Compatible (custom)",
    ];
    let selection = Select::new()
        .with_prompt("Select LLM provider")
        .items(providers)
        .default(0)
        .interact()?;

    match selection {
        0 => {
            state.provider = Some(ProviderKind::Ollama);
            state.base_url = Some(
                Input::new()
                    .with_prompt("Ollama base URL")
                    .default("http://localhost:11434".into())
                    .interact_text()?,
            );
            state.model = Some(
                Input::new()
                    .with_prompt("Model name")
                    .default("qwen3:8b".into())
                    .interact_text()?,
            );
        }
        1 => {
            state.provider = Some(ProviderKind::Claude);
            if !use_age {
                let raw = Password::new().with_prompt("Claude API key").interact()?;
                state.api_key = if raw.is_empty() { None } else { Some(raw) };
            }
            state.model = Some(
                Input::new()
                    .with_prompt("Model name")
                    .default("claude-sonnet-4-5-20250929".into())
                    .interact_text()?,
            );
            let thinking_mode = Select::new()
                .with_prompt("Enable thinking?")
                .items(["No", "Extended", "Adaptive"])
                .default(0)
                .interact()?;
            state.thinking = match thinking_mode {
                1 => {
                    let budget: u32 = Input::new()
                        .with_prompt("Budget tokens (1024-128000)")
                        .default(10_000)
                        .interact_text()?;
                    Some(ThinkingConfig::Extended {
                        budget_tokens: budget,
                    })
                }
                2 => {
                    let effort_idx = Select::new()
                        .with_prompt("Effort level")
                        .items(["Low", "Medium", "High"])
                        .default(1)
                        .interact()?;
                    let effort = match effort_idx {
                        0 => ThinkingEffort::Low,
                        2 => ThinkingEffort::High,
                        _ => ThinkingEffort::Medium,
                    };
                    Some(ThinkingConfig::Adaptive {
                        effort: Some(effort),
                    })
                }
                _ => None,
            };
            state.enable_extended_context = Confirm::new()
                .with_prompt("Enable 1M extended context? (long-context pricing above 200K tokens)")
                .default(false)
                .interact()?;
        }
        2 => {
            state.provider = Some(ProviderKind::OpenAi);
            if !use_age {
                let raw = Password::new().with_prompt("OpenAI API key").interact()?;
                state.api_key = if raw.is_empty() { None } else { Some(raw) };
            }
            state.base_url = Some(
                Input::new()
                    .with_prompt("Base URL")
                    .default("https://api.openai.com/v1".into())
                    .interact_text()?,
            );
            state.model = Some(
                Input::new()
                    .with_prompt("Model name")
                    .default("gpt-4o".into())
                    .interact_text()?,
            );
        }
        3 => {
            state.provider = Some(ProviderKind::Gemini);
            if !use_age {
                let raw = Password::new().with_prompt("Gemini API key").interact()?;
                state.api_key = if raw.is_empty() { None } else { Some(raw) };
            }
            state.model = Some(
                Input::new()
                    .with_prompt("Model name")
                    .default("gemini-2.0-flash".into())
                    .interact_text()?,
            );
            let thinking_opts = [
                "skip (no thinking_level)",
                "minimal",
                "low",
                "medium",
                "high",
            ];
            let thinking_sel = Select::new()
                .with_prompt("Thinking level (for Gemini 3+ thinking models; skip for 2.x)")
                .items(thinking_opts)
                .default(0)
                .interact()?;
            state.gemini_thinking_level = match thinking_sel {
                1 => Some(GeminiThinkingLevel::Minimal),
                2 => Some(GeminiThinkingLevel::Low),
                3 => Some(GeminiThinkingLevel::Medium),
                4 => Some(GeminiThinkingLevel::High),
                _ => None,
            };
        }
        4 => {
            state.pool_mode = true;
            println!("\nConfigure primary provider:");
            let (pk, pb, pm, pa, pn) = prompt_provider_config("Primary")?;
            state.orchestrator_primary_provider = Some(pk);
            state.orchestrator_primary_base_url = pb;
            state.orchestrator_primary_model = Some(pm);
            state.orchestrator_primary_api_key = pa;
            state.orchestrator_primary_compatible_name = pn;
            state.provider = Some(pk);

            println!("\nConfigure fallback provider:");
            let (fk, fb, fm, fa, fn_) = prompt_provider_config("Fallback")?;
            state.orchestrator_fallback_provider = Some(fk);
            state.orchestrator_fallback_base_url = fb;
            state.orchestrator_fallback_model = Some(fm);
            state.orchestrator_fallback_api_key = fa;
            state.orchestrator_fallback_compatible_name = fn_;

            // Use primary model as the top-level model for display purposes
            state.model = state.orchestrator_primary_model.clone();
            state.base_url = state.orchestrator_primary_base_url.clone();
        }
        5 => {
            state.provider = Some(ProviderKind::Compatible);
            state.compatible_name =
                Some(Input::new().with_prompt("Provider name").interact_text()?);
            state.base_url = Some(Input::new().with_prompt("Base URL").interact_text()?);
            state.model = Some(Input::new().with_prompt("Model name").interact_text()?);
            if !use_age {
                state.api_key = Some(
                    Password::new()
                        .with_prompt("API key (leave empty if none)")
                        .allow_empty_password(true)
                        .interact()?,
                );
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn step_memory(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 3/10: Memory ==\n");

    state.sqlite_path = Some(
        Input::new()
            .with_prompt("SQLite database path")
            .default(zeph_core::config::default_sqlite_path())
            .interact_text()?,
    );

    state.sessions_max_history = Input::new()
        .with_prompt("Maximum number of sessions to list (0 = unlimited)")
        .default(100usize)
        .interact_text()?;

    state.sessions_title_max_chars = Input::new()
        .with_prompt("Maximum characters for auto-generated session titles")
        .default(60usize)
        .interact_text()?;

    state.semantic_enabled = Confirm::new()
        .with_prompt("Enable semantic memory (requires Qdrant)?")
        .default(true)
        .interact()?;

    if state.semantic_enabled {
        state.qdrant_url = Some(
            Input::new()
                .with_prompt("Qdrant URL")
                .default("http://localhost:6334".into())
                .interact_text()?,
        );
    }

    state.soft_compaction_threshold = Input::new()
        .with_prompt(
            "Soft compaction threshold: prune tool outputs + apply deferred summaries \
             when context usage exceeds this fraction \
             (0.0-1.0, recommended: below 0.90 — the default hard threshold)",
        )
        .default(state.soft_compaction_threshold)
        .validate_with(|v: &f32| {
            if v.is_finite() && *v > 0.0 && *v < 1.0 {
                Ok(())
            } else {
                Err("must be between 0.0 and 1.0 exclusive")
            }
        })
        .interact_text()?;
    // Loop required for cross-field validation (hard > soft): dialoguer's validate_with
    // closure only sees the parsed value, not external state, so we handle the constraint here.
    loop {
        let soft = state.soft_compaction_threshold;
        let val: f32 = Input::new()
            .with_prompt(format!(
                "Hard compaction threshold: full LLM summarization when context usage exceeds \
                 this fraction (0.0-1.0, must be above soft threshold {soft})"
            ))
            .default(state.hard_compaction_threshold)
            .validate_with(|v: &f32| {
                if v.is_finite() && *v > 0.0 && *v < 1.0 {
                    Ok(())
                } else {
                    Err("must be between 0.0 and 1.0 exclusive")
                }
            })
            .interact_text()?;
        if val > soft {
            state.hard_compaction_threshold = val;
            break;
        }
        eprintln!("error: hard threshold must be greater than soft threshold ({soft}), got {val}",);
    }

    state.graph_memory_enabled = Confirm::new()
        .with_prompt("Enable knowledge graph memory? (experimental)")
        .default(false)
        .interact()?;

    if state.graph_memory_enabled {
        let model: String = Input::new()
            .with_prompt("LLM model for entity extraction (empty = same as agent)")
            .default(String::new())
            .interact_text()?;
        if !model.is_empty() {
            state.graph_extract_model = Some(model);
        }

        state.graph_spreading_activation_enabled = Confirm::new()
            .with_prompt(
                "Enable SYNAPSE spreading activation for graph recall? \
                 (replaces BFS; uses temporal decay + lateral inhibition; recommended defaults: \
                 decay_lambda=0.85, max_hops=3)",
            )
            .default(false)
            .interact()?;
    }

    state.compression_guidelines_enabled = Confirm::new()
        .with_prompt(
            "Enable ACON failure-driven compression guidelines? \
             (learns compression rules from detected context-loss events, \
             requires compression-guidelines feature)",
        )
        .default(false)
        .interact()?;

    state.server_compaction_enabled = Confirm::new()
        .with_prompt(
            "Enable Claude server-side context compaction? (compact-2026-01-12 beta, Claude only)",
        )
        .default(false)
        .interact()?;

    state.shutdown_summary = Confirm::new()
        .with_prompt(
            "Store a session summary on shutdown? (enables cross-session recall for short sessions, \
             advanced params shutdown_summary_min_messages and shutdown_summary_max_messages \
             are config-file-only)",
        )
        .default(true)
        .interact()?;

    state.digest_enabled = Confirm::new()
        .with_prompt(
            "Enable session digest generation? (generates a compact summary of key facts and \
             decisions at session end and injects it at the start of the next session)",
        )
        .default(false)
        .interact()?;

    let strategy_options = ["full_history", "adaptive", "memory_first"];
    let strategy_idx = Select::new()
        .with_prompt(
            "Context assembly strategy (full_history: current behavior; adaptive: switches to \
             memory-first after crossover_turn_threshold turns; memory_first: always use memory \
             instead of full history)",
        )
        .items(strategy_options)
        .default(0)
        .interact()?;
    strategy_options[strategy_idx].clone_into(&mut state.context_strategy);

    println!();
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn step_context_compression(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Context Compression ==\n");
    println!(
        "Active context compression reduces token usage by pruning stale tool outputs \
         and compressing exploration phases.\n"
    );

    state.focus_enabled = Confirm::new()
        .with_prompt("Enable Focus Agent? (LLM-driven exploration bracketing)")
        .default(false)
        .interact()?;

    if state.focus_enabled {
        state.focus_compression_interval = Input::new()
            .with_prompt("Focus compression interval (turns between suggestions)")
            .default(state.focus_compression_interval)
            .validate_with(
                |v: &usize| {
                    if *v >= 1 { Ok(()) } else { Err("must be >= 1") }
                },
            )
            .interact_text()?;
    }

    state.memory_tiers_enabled = Confirm::new()
        .with_prompt(
            "Enable AOI three-layer memory tiers? (episodic -> semantic promotion via LLM)",
        )
        .default(false)
        .interact()?;

    if state.memory_tiers_enabled {
        state.memory_tiers_promotion_min_sessions = Input::new()
            .with_prompt("Minimum sessions before episodic fact is promoted to semantic")
            .default(state.memory_tiers_promotion_min_sessions)
            .validate_with(|v: &u32| if *v >= 2 { Ok(()) } else { Err("must be >= 2") })
            .interact_text()?;
    }

    state.sidequest_enabled = Confirm::new()
        .with_prompt("Enable SideQuest eviction? (LLM-driven tool output eviction)")
        .default(false)
        .interact()?;

    if state.sidequest_enabled {
        state.sidequest_interval_turns = Input::new()
            .with_prompt("SideQuest eviction interval (user turns)")
            .default(state.sidequest_interval_turns)
            .validate_with(|v: &u32| if *v >= 1 { Ok(()) } else { Err("must be >= 1") })
            .interact_text()?;
    }

    let strategy_options = &[
        "reactive (oldest-first, default)",
        "task_aware (keyword relevance scoring)",
        "mig (relevance minus redundancy)",
        "task_aware_mig (combined goal + MIG)",
        "subgoal (HiAgent subgoal-aware, LLM extraction per turn)",
        "subgoal_mig (subgoal + MIG redundancy scoring)",
    ];
    let default_idx = match state.pruning_strategy.as_str() {
        "task_aware" => 1,
        "mig" => 2,
        "task_aware_mig" => 3,
        "subgoal" => 4,
        "subgoal_mig" => 5,
        _ => 0,
    };
    let idx = Select::new()
        .with_prompt("Pruning strategy")
        .items(strategy_options)
        .default(default_idx)
        .interact()?;
    state.pruning_strategy = match idx {
        1 => "task_aware".into(),
        2 => "mig".into(),
        3 => "task_aware_mig".into(),
        4 => "subgoal".into(),
        5 => "subgoal_mig".into(),
        _ => "reactive".into(),
    };

    state.probe_enabled = Confirm::new()
        .with_prompt(
            "Enable compaction probe? (validates summary quality before committing, \
             adds 2 LLM calls per compaction)",
        )
        .default(false)
        .interact()?;

    if state.probe_enabled {
        let provider: String = Input::new()
            .with_prompt(
                "Provider name for probe LLM calls from [[llm.providers]] \
                 (empty = same as summary provider)",
            )
            .default(String::new())
            .interact_text()?;
        if !provider.is_empty() {
            state.probe_provider = Some(provider);
        }

        state.probe_threshold = Input::new()
            .with_prompt("Probe pass threshold (0.0-1.0, scores below this trigger warnings)")
            .default(state.probe_threshold)
            .validate_with(|v: &f32| {
                if v.is_finite() && *v > 0.0 && *v <= 1.0 {
                    Ok(())
                } else {
                    Err("must be in (0.0, 1.0]")
                }
            })
            .interact_text()?;

        loop {
            let threshold = state.probe_threshold;
            let val: f32 = Input::new()
                .with_prompt(format!(
                    "Probe hard-fail threshold (0.0-1.0, scores below this block compaction, \
                     must be below {threshold})"
                ))
                .default(state.probe_hard_fail_threshold)
                .validate_with(|v: &f32| {
                    if v.is_finite() && *v >= 0.0 && *v < 1.0 {
                        Ok(())
                    } else {
                        Err("must be in [0.0, 1.0)")
                    }
                })
                .interact_text()?;
            if val < threshold {
                state.probe_hard_fail_threshold = val;
                break;
            }
            eprintln!(
                "error: hard-fail threshold must be less than pass threshold ({threshold}), got {val}",
            );
        }
    }

    println!();
    Ok(())
}

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
    let provider = state.provider.unwrap_or(ProviderKind::Ollama);

    // Build the providers pool.
    let providers = if state.pool_mode {
        // Multi-provider pool: primary + fallback entries.
        let mut pool = Vec::new();
        if let (Some(pk), Some(pm)) = (
            state.orchestrator_primary_provider,
            state.orchestrator_primary_model.clone(),
        ) {
            pool.push(ProviderEntry {
                provider_type: pk,
                name: state.orchestrator_primary_compatible_name.clone(),
                model: Some(pm),
                base_url: state.orchestrator_primary_base_url.clone(),
                max_tokens: match pk {
                    ProviderKind::Claude => Some(8096),
                    ProviderKind::Gemini => Some(8192),
                    _ => None,
                },
                default: true,
                ..ProviderEntry::default()
            });
        }
        if let (Some(fk), Some(fm)) = (
            state.orchestrator_fallback_provider,
            state.orchestrator_fallback_model.clone(),
        ) {
            pool.push(ProviderEntry {
                provider_type: fk,
                name: state.orchestrator_fallback_compatible_name.clone(),
                model: Some(fm),
                base_url: state.orchestrator_fallback_base_url.clone(),
                max_tokens: match fk {
                    ProviderKind::Claude => Some(8096),
                    ProviderKind::Gemini => Some(8192),
                    _ => None,
                },
                ..ProviderEntry::default()
            });
        }
        pool
    } else {
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
    };

    config.memory = MemoryConfig {
        sqlite_path: state
            .sqlite_path
            .clone()
            .unwrap_or_else(zeph_core::config::default_sqlite_path),
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
            });
        }
        ChannelChoice::Discord => {
            config.discord = Some(DiscordConfig {
                token: None,
                application_id: state.discord_app_id.clone(),
                allowed_user_ids: vec![],
                allowed_role_ids: vec![],
                allowed_channel_ids: vec![],
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
                config
                    .skills
                    .learning
                    .feedback_provider
                    .clone_from(provider);
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
        planner_provider: state
            .orchestration_planner_provider
            .clone()
            .unwrap_or_default(),
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
    config.skills.trust.scan_on_load = state.skill_scan_on_load;

    #[cfg(feature = "guardrail")]
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

    #[cfg(feature = "policy-enforcer")]
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

    #[cfg(feature = "lsp-context")]
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
            tool_allowlist: Vec::new(),
            expected_tools: Vec::new(),
        });
    }
    for server in state.mcp_remote_servers.clone() {
        config.mcp.servers.push(server);
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

fn step_orchestration(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Orchestration (/plan command) ==\n");

    state.orchestration_enabled = Confirm::new()
        .with_prompt("Enable task orchestration? (enables the /plan command)")
        .default(false)
        .interact()?;

    if state.orchestration_enabled {
        state.orchestration_max_tasks = Input::new()
            .with_prompt("Maximum tasks per plan")
            .default(20u32)
            .interact_text()?;

        state.orchestration_max_parallel = Input::new()
            .with_prompt("Maximum parallel tasks")
            .default(4u32)
            .interact_text()?;

        // MF6: warn if max_parallel > max_tasks.
        if state.orchestration_max_parallel > state.orchestration_max_tasks {
            println!(
                "Warning: max_parallel ({}) is greater than max_tasks ({}). \
                 Setting max_parallel = max_tasks.",
                state.orchestration_max_parallel, state.orchestration_max_tasks
            );
            state.orchestration_max_parallel = state.orchestration_max_tasks;
        }

        state.orchestration_confirm_before_execute = Confirm::new()
            .with_prompt("Require confirmation before executing plans?")
            .default(true)
            .interact()?;

        let strategies = ["abort", "retry", "skip", "ask"];
        let strategy_idx = Select::new()
            .with_prompt("Default failure strategy")
            .items(strategies)
            .default(0)
            .interact()?;
        state.orchestration_failure_strategy = strategies[strategy_idx].into();

        let provider: String = Input::new()
            .with_prompt("Provider name for planning LLM calls (empty = primary provider)")
            .default(String::new())
            .interact_text()?;
        // Validate provider name: alphanumeric + `-_`, max 64 chars.
        state.orchestration_planner_provider = if provider.is_empty() {
            None
        } else if provider.len() > 64
            || !provider
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            println!(
                "Warning: provider name contains invalid characters or exceeds 64 chars. \
                 Ignoring and using the primary provider."
            );
            None
        } else {
            Some(provider)
        };
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

fn step_mcpls(state: &mut WizardState) -> anyhow::Result<()> {
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

/// Returns `true` if `mcpls` exists as an executable file on PATH.
///
/// Uses a PATH walk rather than spawning the process to avoid blocking the wizard
/// on a broken binary that enters an infinite loop.
fn mcpls_in_path() -> bool {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let exe_name = if cfg!(windows) { "mcpls.exe" } else { "mcpls" };
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(exe_name))
        .any(|p| p.is_file())
}

/// Writes `.zeph/mcpls.toml` next to `config_path` so that `mcpls --config .zeph/mcpls.toml`
/// starts with the configured workspace roots and language server definitions.
///
/// # Errors
///
/// Returns an error if the directory cannot be created or the file cannot be written.
fn write_mcpls_config(state: &WizardState, config_path: &std::path::Path) -> anyhow::Result<()> {
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
    std::fs::write(&mcpls_path, content)?;
    println!("mcpls config written to {}", mcpls_path.display());

    Ok(())
}

#[allow(clippy::too_many_lines)]
fn step_mcp_remote(state: &mut WizardState) -> anyhow::Result<()> {
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
            policy: zeph_mcp::McpPolicy::default(),
            headers,
            oauth,
            trust_level,
            tool_allowlist: Vec::new(),
            expected_tools: Vec::new(),
        });

        println!("Server added.");
    }

    println!();
    Ok(())
}

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

fn step_agents(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 9/10: Sub-Agent Defaults ==\n");

    let modes = ["default", "accept_edits", "dont_ask"];
    let sel = Select::new()
        .with_prompt("Default permission mode for sub-agents")
        .items(modes)
        .default(0)
        .interact()?;
    state.agents_default_permission_mode = match sel {
        1 => Some(PermissionMode::AcceptEdits),
        2 => Some(PermissionMode::DontAsk),
        _ => None,
    };

    let tools_raw: String = Input::new()
        .with_prompt("Globally disallowed tools (comma-separated, leave empty for none)")
        .default(String::new())
        .interact_text()?;
    state.agents_default_disallowed_tools = tools_raw
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();

    state.agents_allow_bypass_permissions = Confirm::new()
        .with_prompt("Allow sub-agents to use bypass_permissions mode?")
        .default(false)
        .interact()?;

    let user_dir_raw: String = Input::new()
        .with_prompt(
            "User-level agents directory (absolute path, leave empty for platform default)",
        )
        .default(String::new())
        .interact_text()?;
    state.agents_user_dir = if user_dir_raw.trim().is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(user_dir_raw.trim()))
    };

    let memory_scopes = ["none", "local", "project", "user"];
    let memory_sel = Select::new()
        .with_prompt("Default memory scope for sub-agents (none = no memory by default)")
        .items(memory_scopes)
        .default(0)
        .interact()?;
    state.agents_default_memory_scope = match memory_sel {
        1 => Some(MemoryScope::Local),
        2 => Some(MemoryScope::Project),
        3 => Some(MemoryScope::User),
        _ => None,
    };

    println!();
    Ok(())
}

fn step_router(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 10/12: Provider Router ==\n");
    println!("Configure adaptive routing when using multiple LLM providers.");
    println!("Note: routing only takes effect when [llm.router].chain has 2+ providers.");
    println!("Skip this step if you use a single provider.\n");

    let strategy_items = &[
        "None (single provider, no routing)",
        "EMA (latency-aware exponential moving average)",
        "Thompson (probabilistic exploration/exploitation)",
        "Cascade (try cheapest provider first, escalate on degenerate output)",
    ];
    let sel = Select::new()
        .with_prompt("Router strategy")
        .items(strategy_items)
        .default(0)
        .interact()?;

    match sel {
        0 => {
            state.router_strategy = None;
        }
        1 => {
            state.router_strategy = Some("ema".into());
        }
        2 => {
            state.router_strategy = Some("thompson".into());
            let custom_path: String = Input::new()
                .with_prompt(
                    "Thompson state file path (leave empty for default ~/.zeph/router_thompson_state.json)",
                )
                .default(String::new())
                .interact_text()?;
            if !custom_path.is_empty() {
                state.router_thompson_state_path = Some(custom_path);
            }
        }
        3 => {
            state.router_strategy = Some("cascade".into());
            let threshold: f64 = Input::new()
                .with_prompt(
                    "Quality threshold [0.0–1.0] — responses below this score trigger escalation",
                )
                .default(0.5_f64)
                .interact_text()?;
            state.router_cascade_quality_threshold = Some(threshold.clamp(0.0, 1.0));
            let max_esc: u8 = Input::new()
                .with_prompt("Max escalations per request (0 = no escalation)")
                .default(2_u8)
                .interact_text()?;
            state.router_cascade_max_escalations = Some(max_esc);
            let cost_tiers_input: String = Input::new()
                .with_prompt(
                    "Cost tiers: comma-separated provider names cheapest first \
                     (empty = use chain order)",
                )
                .default(String::new())
                .interact_text()?;
            let tiers: Vec<String> = cost_tiers_input
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if !tiers.is_empty() {
                state.router_cascade_cost_tiers = Some(tiers);
            }
        }
        _ => unreachable!(),
    }

    println!();
    Ok(())
}

fn step_learning(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 11/12: Feedback Detector ==\n");

    let detector_items = &[
        "regex (default — pattern matching, no LLM)",
        "judge (LLM-based verification)",
        "model (ML classifier via classifiers feature)",
    ];
    let sel = Select::new()
        .with_prompt("Feedback detector mode")
        .items(detector_items)
        .default(0)
        .interact()?;

    match sel {
        1 => {
            state.detector_mode = Some("judge".into());
            let judge_model: String = Input::new()
                .with_prompt(
                    "Judge model name (e.g. claude-sonnet-4-6; leave empty to use primary provider)",
                )
                .default(String::new())
                .interact_text()?;
            if !judge_model.is_empty() {
                state.judge_model = Some(judge_model);
            }
        }
        2 => {
            state.detector_mode = Some("model".into());
            let feedback_provider: String = Input::new()
                .with_prompt(
                    "Provider name from [[llm.providers]] for feedback detection (leave empty to use primary provider)",
                )
                .default(String::new())
                .interact_text()?;
            if !feedback_provider.is_empty() {
                state.feedback_provider = Some(feedback_provider);
            }
        }
        _ => {
            state.detector_mode = Some("regex".into());
        }
    }

    println!();
    Ok(())
}

fn step_security(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Security ==\n");
    println!(
        "Memory write validation is enabled by default (size limits, forbidden patterns, entity PII scan).\n"
    );
    state.pii_filter_enabled = Confirm::new()
        .with_prompt(
            "Enable PII filter? (scrubs emails, phone numbers, SSNs, and credit card numbers from tool outputs before LLM context and debug dumps)",
        )
        .default(false)
        .interact()?;
    state.rate_limit_enabled = Confirm::new()
        .with_prompt(
            "Enable tool rate limiter? (sliding-window per-category limits: shell 30/min, web 20/min, memory 60/min)",
        )
        .default(false)
        .interact()?;
    state.skill_scan_on_load = Confirm::new()
        .with_prompt(
            "Scan skill content for injection patterns on load? (advisory — logs warnings, does not block; recommended)",
        )
        .default(true)
        .interact()?;
    state.pre_execution_verify_enabled = Confirm::new()
        .with_prompt(
            "Enable pre-execution verification? (blocks destructive commands like rm -rf / and injection patterns before tool execution; recommended)",
        )
        .default(true)
        .interact()?;
    if state.pre_execution_verify_enabled {
        println!("  Shell tools checked: bash, shell, terminal (configurable in config.toml)");
        let paths_input: String = Input::new()
            .with_prompt(
                "Allowed paths for destructive commands (comma-separated, empty = deny all)",
            )
            .allow_empty(true)
            .interact_text()?;
        state.pre_execution_verify_allowed_paths = paths_input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    #[cfg(feature = "guardrail")]
    {
        state.guardrail_enabled = Confirm::new()
            .with_prompt(
                "Enable LLM-based guardrail? (prompt injection pre-screening via a dedicated safety model, e.g. llama-guard)",
            )
            .default(false)
            .interact()?;

        if state.guardrail_enabled {
            let provider_options = &["ollama", "claude", "openai", "compatible"];
            let provider_idx = dialoguer::Select::new()
                .with_prompt("Guardrail provider")
                .items(provider_options)
                .default(0)
                .interact()?;
            provider_options[provider_idx].clone_into(&mut state.guardrail_provider);

            state.guardrail_model = dialoguer::Input::new()
                .with_prompt("Guardrail model")
                .default(if state.guardrail_provider == "ollama" {
                    "llama-guard-3:1b".to_owned()
                } else {
                    String::new()
                })
                .allow_empty(true)
                .interact_text()?;

            let action_options = &["block", "warn"];
            let action_idx = dialoguer::Select::new()
                .with_prompt("Action on flagged input")
                .items(action_options)
                .default(0)
                .interact()?;
            action_options[action_idx].clone_into(&mut state.guardrail_action);

            let timeout_str: String = dialoguer::Input::new()
                .with_prompt("Guardrail timeout (ms)")
                .default("500".to_owned())
                .interact_text()?;
            state.guardrail_timeout_ms = timeout_str.parse().unwrap_or(500);
        }
    }

    #[cfg(feature = "classifiers")]
    {
        state.classifiers_enabled = Confirm::new()
            .with_prompt(
                "Enable ML classifiers? (injection detection and PII detection via candle inference)",
            )
            .default(false)
            .interact()?;

        if state.classifiers_enabled {
            state.pii_enabled = Confirm::new()
                .with_prompt("Enable PII detection? (NER-based scan of assistant responses)")
                .default(false)
                .interact()?;
        }
    }

    println!();
    Ok(())
}

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

fn step_policy(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Policy Enforcer ==\n");
    println!(
        "Declarative tool call authorization via TOML rules (requires policy-enforcer feature).\n"
    );

    state.policy_enforcer_enabled = Confirm::new()
        .with_prompt("Enable policy enforcer?")
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
    std::fs::write(&path, &toml_str)?;
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

    if state.pool_mode {
        collect_provider_secret(
            &mut secrets,
            state.orchestrator_primary_provider,
            state.orchestrator_primary_api_key.as_ref(),
            state.orchestrator_primary_compatible_name.as_deref(),
            use_age,
        );
        collect_provider_secret(
            &mut secrets,
            state.orchestrator_fallback_provider,
            state.orchestrator_fallback_api_key.as_ref(),
            state.orchestrator_fallback_compatible_name.as_deref(),
            use_age,
        );
    } else {
        collect_provider_secret(
            &mut secrets,
            state.provider,
            state.api_key.as_ref(),
            state.compatible_name.as_deref(),
            use_age,
        );
    }

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

    fn pool_mode_state() -> WizardState {
        WizardState {
            pool_mode: true,
            provider: Some(ProviderKind::Claude),
            model: Some("claude-sonnet-4-5-20250929".into()),
            embedding_model: Some("qwen3-embedding".into()),
            orchestrator_primary_provider: Some(ProviderKind::Claude),
            orchestrator_primary_model: Some("claude-sonnet-4-5-20250929".into()),
            orchestrator_primary_api_key: Some("key-abc".into()),
            orchestrator_fallback_provider: Some(ProviderKind::Ollama),
            orchestrator_fallback_model: Some("qwen3:8b".into()),
            orchestrator_fallback_base_url: Some("http://localhost:11434".into()),
            vault_backend: "env".into(),
            semantic_enabled: true,
            ..WizardState::default()
        }
    }

    #[test]
    fn build_config_pool_mode_creates_provider_pool() {
        let state = pool_mode_state();
        let config = build_config(&state);
        assert_eq!(config.llm.providers.len(), 2);
        assert!(config.llm.providers[0].default);
        assert_eq!(config.llm.providers[0].provider_type, ProviderKind::Claude);
        assert_eq!(
            config.llm.providers[0].model.as_deref(),
            Some("claude-sonnet-4-5-20250929")
        );
        assert_eq!(config.llm.providers[1].provider_type, ProviderKind::Ollama);
        assert_eq!(config.llm.providers[1].model.as_deref(), Some("qwen3:8b"));
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
    fn build_pool_mode_without_primary_yields_empty_pool() {
        let state = WizardState {
            pool_mode: true,
            orchestrator_primary_provider: None,
            orchestrator_fallback_provider: Some(ProviderKind::Ollama),
            orchestrator_fallback_model: Some("qwen3:8b".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        // Without a primary provider, pool has only the fallback entry.
        assert_eq!(config.llm.providers.len(), 1);
        assert_eq!(config.llm.providers[0].provider_type, ProviderKind::Ollama);
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
        assert_eq!(config.memory.soft_compaction_threshold, 0.60);
        assert_eq!(config.memory.hard_compaction_threshold, 0.85);
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
        assert_eq!(config.memory.soft_compaction_threshold, 0.70);
        assert_eq!(config.memory.hard_compaction_threshold, 0.90);
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
        assert_eq!(config.memory.soft_compaction_threshold, 0.80);
        assert_eq!(config.memory.hard_compaction_threshold, 0.60);
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
        assert_eq!(config.memory.soft_compaction_threshold, 0.70);
        assert_eq!(config.memory.hard_compaction_threshold, 1.0);
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
}
