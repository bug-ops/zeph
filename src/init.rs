// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use dialoguer::{Confirm, Input, Password, Select};
use zeph_core::config::{
    AcpConfig, CloudLlmConfig, CompatibleConfig, Config, DiscordConfig, LlmConfig, MemoryConfig,
    OrchestrationConfig, OrchestratorConfig, OrchestratorProviderConfig, ProviderKind,
    RouterConfig, RouterStrategyConfig, SemanticConfig, SessionsConfig, SlackConfig,
    TelegramConfig, VaultConfig,
};
use zeph_core::subagent::def::{MemoryScope, PermissionMode};
use zeph_llm::{ThinkingConfig, ThinkingEffort};

#[derive(Default)]
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
    pub(crate) agents_default_permission_mode: Option<PermissionMode>,
    pub(crate) agents_default_disallowed_tools: Vec<String>,
    pub(crate) agents_allow_bypass_permissions: bool,
    /// Custom user-level agents directory (empty = use platform default).
    pub(crate) agents_user_dir: Option<std::path::PathBuf>,
    /// Default memory scope for sub-agents (None = no memory by default).
    pub(crate) agents_default_memory_scope: Option<MemoryScope>,
    /// "regex" or "judge" — defaults to "regex" (no LLM calls).
    pub(crate) detector_mode: Option<String>,
    pub(crate) judge_model: Option<String>,
    /// Router strategy: None = no router, "ema", or "thompson".
    pub(crate) router_strategy: Option<String>,
    /// Custom path for Thompson state file (None = use default).
    pub(crate) router_thompson_state_path: Option<String>,
    // Orchestration settings
    pub(crate) orchestration_enabled: bool,
    pub(crate) orchestration_max_tasks: u32,
    pub(crate) orchestration_max_parallel: u32,
    pub(crate) orchestration_confirm_before_execute: bool,
    pub(crate) orchestration_failure_strategy: String,
    pub(crate) orchestration_planner_model: Option<String>,
    // Debug settings
    pub(crate) debug_dump_enabled: bool,
    // Graph memory settings
    pub(crate) graph_memory_enabled: bool,
    pub(crate) graph_extract_model: Option<String>,
    pub(crate) deferred_apply_threshold: f32,
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
        deferred_apply_threshold: 0.70,
        ..WizardState::default()
    };

    step_vault(&mut state)?;
    step_llm(&mut state)?;
    step_memory(&mut state)?;
    step_channel(&mut state)?;
    step_update_check(&mut state)?;
    step_scheduler(&mut state)?;
    step_orchestration(&mut state)?;
    step_daemon(&mut state)?;
    step_acp(&mut state)?;
    step_agents(&mut state)?;
    step_router(&mut state)?;
    step_learning(&mut state)?;
    step_debug(&mut state)?;
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
            state.provider = Some(ProviderKind::Orchestrator);
            println!("\nConfigure primary provider:");
            let (pk, pb, pm, pa, pn) = prompt_provider_config("Primary")?;
            state.orchestrator_primary_provider = Some(pk);
            state.orchestrator_primary_base_url = pb;
            state.orchestrator_primary_model = Some(pm);
            state.orchestrator_primary_api_key = pa;
            state.orchestrator_primary_compatible_name = pn;

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
        4 => {
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

fn step_memory(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 3/10: Memory ==\n");

    state.sqlite_path = Some(
        Input::new()
            .with_prompt("SQLite database path")
            .default("./data/zeph.db".into())
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

    state.deferred_apply_threshold = Input::new()
        .with_prompt(
            "Apply deferred tool summaries when context usage exceeds this fraction (0.0-1.0, \
             must be below compaction threshold 0.80)",
        )
        .default(0.70f32)
        .validate_with(|v: &f32| {
            if *v > 0.0 && *v < 1.0 {
                Ok(())
            } else {
                Err("must be between 0.0 and 1.0 exclusive")
            }
        })
        .interact_text()?;

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

    let orchestrator = if provider == ProviderKind::Orchestrator {
        build_orchestrator_config(state)
    } else {
        None
    };

    config.llm = LlmConfig {
        provider,
        base_url: state
            .base_url
            .clone()
            .unwrap_or_else(|| "http://localhost:11434".into()),
        model: state.model.clone().unwrap_or_else(|| "qwen3:8b".into()),
        embedding_model: state
            .embedding_model
            .clone()
            .unwrap_or_else(|| "qwen3-embedding".into()),
        cloud: if provider == ProviderKind::Claude {
            Some(CloudLlmConfig {
                model: state
                    .model
                    .clone()
                    .unwrap_or_else(|| "claude-sonnet-4-5-20250929".into()),
                max_tokens: 8096,
                thinking: state.thinking.clone(),
            })
        } else {
            None
        },
        ollama: None,
        openai: None,
        candle: None,
        orchestrator,
        compatible: if provider == ProviderKind::Compatible {
            Some(vec![CompatibleConfig {
                name: state
                    .compatible_name
                    .clone()
                    .unwrap_or_else(|| "custom".into()),
                base_url: state.base_url.clone().unwrap_or_default(),
                model: state.model.clone().unwrap_or_default(),
                max_tokens: 4096,
                embedding_model: None,
            }])
        } else {
            None
        },
        router: state.router_strategy.as_deref().map(|s| RouterConfig {
            chain: vec![],
            strategy: match s {
                "thompson" => RouterStrategyConfig::Thompson,
                _ => RouterStrategyConfig::Ema,
            },
            thompson_state_path: state.router_thompson_state_path.clone(),
        }),
        stt: None,
        vision_model: state.vision_model.clone().filter(|s| !s.is_empty()),
        response_cache_enabled: false,
        response_cache_ttl_secs: 3600,
        router_ema_enabled: state.router_strategy.as_deref().is_some_and(|s| s == "ema"),
        router_ema_alpha: 0.1,
        router_reorder_interval: 10,
        instruction_file: None,
    };

    config.memory = MemoryConfig {
        sqlite_path: state
            .sqlite_path
            .clone()
            .unwrap_or_else(|| "./data/zeph.db".into()),
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
    config.memory.deferred_apply_threshold = state.deferred_apply_threshold;
    if config.memory.deferred_apply_threshold >= config.memory.compaction_threshold {
        eprintln!(
            "warning: deferred_apply_threshold ({}) is not less than compaction_threshold ({}); \
             deferred summaries will fire after compaction, reducing cache savings",
            config.memory.deferred_apply_threshold, config.memory.compaction_threshold,
        );
    }

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

    if state.detector_mode.as_deref() == Some("judge") {
        config.skills.learning.detector_mode = zeph_core::config::DetectorMode::Judge;
        if let Some(ref model) = state.judge_model {
            config.skills.learning.judge_model.clone_from(model);
        }
    }

    config.orchestration = OrchestrationConfig {
        enabled: state.orchestration_enabled,
        max_tasks: state.orchestration_max_tasks,
        max_parallel: state.orchestration_max_parallel,
        confirm_before_execute: state.orchestration_confirm_before_execute,
        default_failure_strategy: state.orchestration_failure_strategy.clone(),
        planner_model: state.orchestration_planner_model.clone(),
        ..OrchestrationConfig::default()
    };

    config.debug.enabled = state.debug_dump_enabled;

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

fn build_orchestrator_config(state: &WizardState) -> Option<OrchestratorConfig> {
    let primary_kind = state.orchestrator_primary_provider?;
    let primary_model = state.orchestrator_primary_model.clone().unwrap_or_default();
    let fallback_kind = state.orchestrator_fallback_provider?;
    let fallback_model = state
        .orchestrator_fallback_model
        .clone()
        .unwrap_or_default();

    let primary_name = primary_kind.as_str().to_owned();
    let fallback_name = if fallback_kind.as_str() == primary_name {
        format!("{}-fallback", fallback_kind.as_str())
    } else {
        fallback_kind.as_str().to_owned()
    };

    let embed_model = state
        .embedding_model
        .clone()
        .unwrap_or_else(|| "qwen3-embedding".into());

    let default_route = format!("{primary_name}/{primary_model}");
    let embed_route = format!("{primary_name}/{embed_model}");

    let mut providers = std::collections::HashMap::new();
    providers.insert(
        primary_name.clone(),
        OrchestratorProviderConfig {
            provider_type: primary_kind.as_str().to_owned(),
            model: Some(primary_model),
            base_url: None,
            embedding_model: None,
            filename: None,
            device: None,
            instruction_file: None,
        },
    );
    providers.insert(
        fallback_name.clone(),
        OrchestratorProviderConfig {
            provider_type: fallback_kind.as_str().to_owned(),
            model: Some(fallback_model),
            base_url: None,
            embedding_model: None,
            filename: None,
            device: None,
            instruction_file: None,
        },
    );

    let mut routes = std::collections::HashMap::new();
    routes.insert("chat".to_owned(), vec![primary_name, fallback_name]);
    routes.insert("embed".to_owned(), vec![embed_route]);

    Some(OrchestratorConfig {
        default: default_route,
        embed: embed_model,
        providers,
        routes,
    })
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

        let model: String = Input::new()
            .with_prompt("Planner model override (leave empty for default)")
            .default(String::new())
            .interact_text()?;
        // SEC-P6-02: validate model name — max 128 chars, alphanumeric + `:.-` only.
        state.orchestration_planner_model = if model.is_empty() {
            None
        } else if model.len() > 128
            || !model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == ':' || c == '.' || c == '-')
        {
            println!(
                "Warning: planner model name contains invalid characters or exceeds 128 chars. \
                 Ignoring and using the default model."
            );
            None
        } else {
            Some(model)
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
    ];
    let sel = Select::new()
        .with_prompt("Feedback detector mode")
        .items(detector_items)
        .default(0)
        .interact()?;

    if sel == 1 {
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
    } else {
        state.detector_mode = Some("regex".into());
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

    print_secrets_instructions(state);
    print_next_steps(state, &path);

    Ok(())
}

fn api_key_env_var(kind: ProviderKind, name: Option<&str>) -> Option<String> {
    match kind {
        ProviderKind::Claude => Some("ZEPH_CLAUDE_API_KEY".to_owned()),
        ProviderKind::OpenAi => Some("ZEPH_OPENAI_API_KEY".to_owned()),
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

    if state.provider == Some(ProviderKind::Orchestrator) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orchestrator_state() -> WizardState {
        WizardState {
            provider: Some(ProviderKind::Orchestrator),
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
    fn build_config_orchestrator_sets_provider() {
        let state = orchestrator_state();
        let config = build_config(&state);
        assert_eq!(config.llm.provider, ProviderKind::Orchestrator);
    }

    #[test]
    fn build_config_orchestrator_generates_orch_config() {
        let state = orchestrator_state();
        let config = build_config(&state);
        let orch = config
            .llm
            .orchestrator
            .expect("orchestrator config present");

        assert!(orch.default.starts_with("claude/"));
        assert!(orch.providers.contains_key("claude"));
        assert!(orch.providers.contains_key("ollama"));

        let claude = &orch.providers["claude"];
        assert_eq!(claude.provider_type, "claude");
        assert_eq!(claude.model.as_deref(), Some("claude-sonnet-4-5-20250929"));

        let ollama = &orch.providers["ollama"];
        assert_eq!(ollama.provider_type, "ollama");
        assert_eq!(ollama.model.as_deref(), Some("qwen3:8b"));

        let chat_route = orch.routes.get("chat").expect("chat route exists");
        assert!(chat_route.contains(&"claude".to_owned()));
        assert!(chat_route.contains(&"ollama".to_owned()));
    }

    #[test]
    fn build_config_orchestrator_embed_route() {
        let state = orchestrator_state();
        let config = build_config(&state);
        let orch = config
            .llm
            .orchestrator
            .expect("orchestrator config present");
        assert!(orch.routes.contains_key("embed"));
        assert_eq!(orch.embed, "qwen3-embedding");
    }

    #[test]
    fn build_config_orchestrator_fallback_name_deduplicated() {
        // When primary and fallback have the same provider kind, fallback gets a suffix
        let state = WizardState {
            provider: Some(ProviderKind::Orchestrator),
            model: Some("qwen3:8b".into()),
            embedding_model: Some("qwen3-embedding".into()),
            orchestrator_primary_provider: Some(ProviderKind::Ollama),
            orchestrator_primary_model: Some("qwen3:8b".into()),
            orchestrator_primary_base_url: Some("http://localhost:11434".into()),
            orchestrator_fallback_provider: Some(ProviderKind::Ollama),
            orchestrator_fallback_model: Some("llama3:8b".into()),
            orchestrator_fallback_base_url: Some("http://localhost:11435".into()),
            vault_backend: "env".into(),
            semantic_enabled: false,
            ..WizardState::default()
        };
        let config = build_config(&state);
        let orch = config
            .llm
            .orchestrator
            .expect("orchestrator config present");
        assert!(
            orch.providers.contains_key("ollama-fallback"),
            "fallback key should have suffix when same as primary"
        );
    }

    #[test]
    fn build_config_non_orchestrator_has_no_orch_config() {
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
        assert!(config.llm.orchestrator.is_none());
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
    fn build_orchestrator_config_returns_none_without_primary() {
        let state = WizardState {
            provider: Some(ProviderKind::Orchestrator),
            orchestrator_primary_provider: None,
            orchestrator_fallback_provider: Some(ProviderKind::Ollama),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(
            config.llm.orchestrator.is_none(),
            "missing primary provider must yield no OrchestratorConfig"
        );
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
}
