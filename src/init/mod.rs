// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use dialoguer::{Confirm, Input, Password, Select};
use zeph_config::{
    BgIsolation, GeminiThinkingLevel, ThinkingConfig, VaultBackend, WorktreeBaseRef,
};
use zeph_core::config::{
    AcpConfig, ChannelSkillsConfig, Config, DiscordConfig, LlmConfig, LlmRoutingStrategy,
    McpServerConfig, McpTrustLevel, MemoryConfig, OrchestrationConfig, ProviderEntry, ProviderKind,
    ProviderName, PruningStrategy, SchedulerSecurityConfig, SemanticConfig, SessionsConfig,
    SlackConfig, TelegramConfig, TriggerPolicy, VaultConfig,
};
use zeph_subagent::def::{MemoryScope, PermissionMode};
use zeroize::Zeroizing;

pub(super) mod agents;
pub(super) mod durable;
pub(super) mod llm;
pub(super) mod mcp;
pub(super) mod memory;
pub(super) mod security;
pub(super) mod session;
pub(super) mod validate;
pub(super) mod worktree;

use agents::{step_agents, step_learning, step_orchestration, step_router};
use durable::step_durable;
use llm::step_llm;
use mcp::{step_mcp_discovery, step_mcp_remote, step_mcpls, write_mcpls_config};
use memory::{step_context_compression, step_memory};
use security::{step_policy, step_sandbox, step_security, step_trajectory};
use session::{step_serve, step_session};
use worktree::step_worktree;

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
    /// Tracks whether the user provided a Qdrant API key during a previous wizard run.
    /// The key itself is never stored here — it must be set via `zeph vault set ZEPH_QDRANT_API_KEY`.
    pub(crate) qdrant_api_key: bool,
    pub(crate) semantic_enabled: bool,
    pub(crate) channel: ChannelChoice,
    pub(crate) telegram_token: Option<String>,
    pub(crate) telegram_users: Vec<String>,
    pub(crate) telegram_stream_interval_ms: u64,
    pub(crate) discord_token: Option<String>,
    pub(crate) discord_app_id: Option<String>,
    /// Discord user snowflakes allowed to interact with the bot. Required (together with
    /// `discord_allowed_role_ids`) since `DiscordChannel::new` now refuses to start when both
    /// are empty (#6472) — the wizard must prompt so `--init` never produces an unstartable
    /// config.
    pub(crate) discord_allowed_user_ids: Vec<String>,
    /// Discord role snowflakes allowed to interact with the bot. See `discord_allowed_user_ids`.
    pub(crate) discord_allowed_role_ids: Vec<String>,
    pub(crate) slack_bot_token: Option<String>,
    pub(crate) slack_signing_secret: Option<String>,
    /// Slack user IDs allowed to interact with the bot. Required since
    /// `SlackChannel::new_with_supervisor` now refuses to start when empty (#6472).
    pub(crate) slack_allowed_user_ids: Vec<String>,
    pub(crate) vault_backend: String,
    pub(crate) auto_update_check: bool,
    pub(crate) scheduler_enabled: bool,
    pub(crate) scheduler_tick_interval_secs: u64,
    pub(crate) scheduler_max_tasks: usize,
    pub(crate) skills_registry_enabled: bool,
    pub(crate) search_enabled: bool,
    pub(crate) daemon_enabled: bool,
    pub(crate) daemon_host: String,
    pub(crate) daemon_port: u16,
    pub(crate) daemon_auth_token: Option<String>,
    pub(crate) acp_enabled: bool,
    pub(crate) acp_agent_name: String,
    pub(crate) acp_agent_version: String,
    pub(crate) acp_additional_directories: Vec<std::path::PathBuf>,
    /// Named bearer-token clients for HTTP/WS multi-tenant isolation (#5868).
    pub(crate) acp_auth_clients: Vec<zeph_config::AcpAuthClient>,
    pub(crate) acp_auth_methods: Vec<zeph_config::AcpAuthMethod>,
    pub(crate) acp_message_ids_enabled: bool,
    pub(crate) acp_subagents_enabled: bool,
    pub(crate) acp_default_temperature_preset: zeph_config::AcpTemperaturePreset,
    pub(crate) thinking: Option<ThinkingConfig>,
    pub(crate) enable_extended_context: bool,
    /// Default `reasoning_effort` for `OpenAI`/Compatible providers (`"low"|"medium"|"high"`).
    /// Claude and Gemini configure their equivalent reasoning depth via `thinking` /
    /// `gemini_thinking_level` above — this field is `OpenAI`-specific to avoid two
    /// conflicting wizard prompts for the same knob on those providers.
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) agents_default_permission_mode: Option<PermissionMode>,
    pub(crate) agents_default_disallowed_tools: Vec<String>,
    pub(crate) agents_allow_bypass_permissions: bool,
    /// Custom user-level agents directory (empty = use platform default).
    pub(crate) agents_user_dir: Option<std::path::PathBuf>,
    /// Default memory scope for sub-agents (None = no memory by default).
    pub(crate) agents_default_memory_scope: Option<MemoryScope>,
    /// Forward each running sub-agent's per-turn text/thinking output to an active consumer
    /// surface (issue #6359). Default `false` (opt-in, matches `SubAgentConfig::default()`).
    pub(crate) agents_forward_transcript: bool,
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
    /// Kills a task if it emits no progress for this many seconds (opt-in; `None` disables).
    /// Must be set above the longest expected single-turn duration (spec-075-orchestration-node-control-parity, #6021, enforced by #6245).
    pub(crate) orchestration_default_idle_timeout_secs: Option<u64>,
    // Ensemble-verified plan verification (spec 073-orch-ensemble-merge, opt-in)
    pub(crate) ensemble_enabled: bool,
    pub(crate) ensemble_members: Vec<String>,
    // Command-style dynamic task handoff (spec-080, #6363, opt-in)
    pub(crate) command_enabled: bool,
    pub(crate) command_max_handoffs: u32,
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
    pub(crate) experiments_eval_provider: String,
    pub(crate) experiments_schedule_enabled: bool,
    pub(crate) experiments_schedule_cron: String,
    // Security
    pub(crate) pii_filter_enabled: bool,
    pub(crate) rate_limit_enabled: bool,
    /// Daily LLM cost cap in US cents (`0` = unlimited). Distinct from
    /// `budget_hint_enabled`, which only injects soft system-prompt hints.
    pub(crate) max_daily_cents: u32,
    pub(crate) skill_scan_on_load: bool,
    pub(crate) skill_require_integrity_check_on_promote: bool,
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
    /// SONAR NLI entailment check stage (#5438).
    pub(crate) nli_enabled: bool,
    /// Provider name from `[[llm.providers]]` for NLI inference; empty = primary provider.
    pub(crate) nli_provider: String,
    /// PAAC secret placeholder masking registry (#5437).
    pub(crate) secret_masking_enabled: bool,
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
    /// Provider name from `[[llm.providers]]` for LLM-assisted policy checks. Empty = disabled.
    pub(crate) policy_provider: String,
    /// Consecutive low-utility tool calls before the loop hard-stops. 0 = disabled.
    pub(crate) utility_window: usize,
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
    // Cross-thread key-value store (spec-080, #6363, opt-in)
    pub(crate) store_enabled: bool,
    pub(crate) store_max_value_bytes: usize,
    // Session recap on resume (#3064)
    pub(crate) recap_on_resume: bool,
    // Resume-visibility banner on CLI/TUI startup (spec-068 §13, #6420)
    pub(crate) resume_show_banner: bool,
    // Install-time plugin/skill name-similarity typosquat check (spec-043, #5864)
    pub(crate) plugins_reputation_enabled: bool,
    // Provider override persistence (#4654)
    pub(crate) persist_provider_overrides: bool,
    // MCP elicitation (#3141)
    pub(crate) mcp_elicitation_enabled: bool,
    pub(crate) mcp_elicitation_warn_sensitive: bool,
    // MARCH self-check pipeline (#3226, #3228)
    pub(crate) quality_self_check: bool,
    pub(crate) quality_trigger: String,
    pub(crate) quality_latency_budget_ms: u64,
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
    // Undo/redo checkpoints (#4990)
    pub(crate) shell_checkpoints_enabled: bool,
    pub(crate) shell_max_checkpoints: usize,
    // File read sandbox (#2525)
    pub(crate) file_deny_read: Vec<String>,
    pub(crate) file_allow_read: Vec<String>,
    // OS subprocess sandbox (#3070, #3077)
    pub(crate) sandbox_enabled: bool,
    pub(crate) sandbox_profile: String,
    pub(crate) sandbox_backend: zeph_config::SandboxBackend,
    pub(crate) sandbox_strict: bool,
    pub(crate) sandbox_allow_read: Vec<String>,
    pub(crate) sandbox_allow_write: Vec<String>,
    /// Hostnames denied egress from sandboxed subprocesses (#3294).
    pub(crate) sandbox_denied_domains: Vec<String>,
    /// Whether to abort startup when no effective OS sandbox is available (#3294).
    pub(crate) sandbox_fail_if_unavailable: bool,
    // Budget hint injection (#2267)
    pub(crate) budget_hint_enabled: bool,
    // Time-reminder injection (#6361)
    pub(crate) time_reminder_enabled: bool,
    pub(crate) time_reminder_interval_requests: u32,
    // SleepGate forgetting sweep (#2397)
    pub(crate) forgetting_enabled: bool,
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
    // Trajectory risk sentinel (spec 050)
    pub(crate) trajectory_critical_at: f32,
    pub(crate) trajectory_auto_recover: u32,
    // Gonka native provider (#3613)
    pub(crate) gonka_private_key: Option<Zeroizing<String>>,
    pub(crate) gonka_address: Option<String>,
    pub(crate) gonka_nodes: Vec<zeph_config::GonkaNode>,
    // Cocoon provider (#3671)
    /// Sidecar URL provided by the wizard (e.g. `http://localhost:10000`).
    pub(crate) cocoon_client_url: Option<String>,
    /// `true` when the user confirmed they have an access hash stored in the vault.
    pub(crate) cocoon_wants_access_hash: bool,
    /// Show TON balance in TUI status bar (spec §15.2 opt-in redaction, #4649).
    pub(crate) cocoon_show_balance: bool,
    // CAM fidelity (#4547)
    /// Enable heuristic fidelity scoring (Full/Compressed/Placeholder).
    pub(crate) fidelity_enabled: bool,
    // MemGuard type-aware retrieval composition (spec 004-16, #6086)
    /// Enable type-aware retrieval composition (`[memory.type_aware_compose]`).
    pub(crate) type_aware_compose_enabled: bool,
    /// Widen the active set per classified query intent when type-aware composition is enabled.
    pub(crate) type_aware_compose_intent_scoped: bool,
    // Worktree isolation for sub-agents (#4656)
    pub(crate) worktree_enabled: bool,
    pub(crate) worktree_bg_isolation: BgIsolation,
    pub(crate) worktree_base_ref: WorktreeBaseRef,
    // Worktree disk-quota + auto-reconcile (#5924)
    /// `None` = unlimited concurrent worktrees.
    pub(crate) worktree_max_worktrees: Option<usize>,
    /// `None` = no disk-usage accounting.
    pub(crate) worktree_disk_quota_mb: Option<u64>,
    /// `0` = periodic reconcile sweep disabled.
    pub(crate) worktree_auto_reconcile_secs: u64,
    // Durable execution layer (spec-064, #4949)
    /// The durable section as configured by the wizard (the AEAD key is stored separately in the
    /// vault, never inline).
    pub(crate) durable: zeph_core::config::DurableConfig,
    /// A freshly generated base64 `ZEPH_DURABLE_KEY`, set when durable execution is enabled. Written
    /// to the age vault during review, never serialized into the config TOML.
    pub(crate) durable_key_b64: Option<String>,
    /// Start every session in ultra-compressed (caveman) output mode (#4985).
    pub(crate) caveman_default_on: bool,
    /// Provider name from `[[llm.providers]]` chosen for knowledge ingest (Phase 2 graph).
    /// Empty = use primary provider.
    pub(crate) knowledge_ingest_provider: String,
    /// Whether to register the `zeph://` URI scheme during `--init` (deep-link feature).
    #[cfg(feature = "deep-link")]
    pub(crate) deep_link_register: bool,
    /// Whether to require confirmation before injecting a deep-link prompt (INV-TRUST).
    #[cfg(feature = "deep-link")]
    pub(crate) deep_link_confirm_before_prompt: bool,
    /// TUI visual theme name (preset or user file).
    pub(crate) tui_theme_name: String,
    /// Terminal colour mode override.
    pub(crate) tui_color_mode: zeph_config::ColorMode,
    /// Whether TUI micro-delights are enabled (tok/s, toasts, flash, scroll, shimmer).
    pub(crate) tui_delights_enabled: bool,
    /// Whether opt-in mouse capture is enabled at startup.
    pub(crate) tui_mouse_enabled: bool,
    // Durable session persistence (spec-068, #5343, P4)
    /// Whether to maintain a durable, replayable JSONL event log per conversation-session.
    pub(crate) session_persistence_enabled: bool,
    /// Directory under which per-session event logs are stored.
    pub(crate) session_data_dir: String,
    // `zeph serve-sessions` HTTP/SSE API (spec-068 §9, #5343, P4)
    pub(crate) serve_http_addr: String,
    pub(crate) serve_require_auth: bool,
    pub(crate) serve_auth_token_vault_key: String,
    pub(crate) serve_max_sessions: usize,
    pub(crate) serve_session_idle_ttl_secs: u64,
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
            qdrant_api_key: false,
            semantic_enabled: false,
            channel: ChannelChoice::default(),
            telegram_token: None,
            telegram_users: Vec::new(),
            telegram_stream_interval_ms: 3000,
            discord_token: None,
            discord_app_id: None,
            discord_allowed_user_ids: Vec::new(),
            discord_allowed_role_ids: Vec::new(),
            slack_bot_token: None,
            slack_signing_secret: None,
            slack_allowed_user_ids: Vec::new(),
            vault_backend: String::new(),
            auto_update_check: false,
            scheduler_enabled: false,
            scheduler_tick_interval_secs: 0,
            scheduler_max_tasks: 0,
            skills_registry_enabled: false,
            search_enabled: false,
            daemon_enabled: false,
            daemon_host: String::new(),
            daemon_port: 0,
            daemon_auth_token: None,
            acp_enabled: false,
            acp_agent_name: String::new(),
            acp_agent_version: String::new(),
            acp_additional_directories: Vec::new(),
            acp_auth_clients: Vec::new(),
            acp_auth_methods: vec![zeph_config::AcpAuthMethod::Agent],
            acp_message_ids_enabled: true,
            acp_subagents_enabled: false,
            acp_default_temperature_preset: zeph_config::AcpTemperaturePreset::default(),
            thinking: None,
            enable_extended_context: false,
            reasoning_effort: None,
            agents_default_permission_mode: None,
            agents_default_disallowed_tools: Vec::new(),
            agents_allow_bypass_permissions: false,
            agents_user_dir: None,
            agents_default_memory_scope: None,
            agents_forward_transcript: false,
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
            orchestration_default_idle_timeout_secs: None,
            ensemble_enabled: false,
            ensemble_members: Vec::new(),
            command_enabled: false,
            command_max_handoffs: 16,
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
            experiments_eval_provider: String::new(),
            experiments_schedule_enabled: false,
            experiments_schedule_cron: String::new(),
            pii_filter_enabled: true,
            rate_limit_enabled: true,
            max_daily_cents: 2500,
            skill_scan_on_load: true,
            skill_require_integrity_check_on_promote: true,
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
            nli_enabled: false,
            nli_provider: String::new(),
            secret_masking_enabled: true,
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
            policy_provider: String::new(),
            utility_window: 0,
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
            store_enabled: false,
            store_max_value_bytes: 65536,
            recap_on_resume: true,
            resume_show_banner: true,
            plugins_reputation_enabled: true,
            persist_provider_overrides: true,
            mcp_elicitation_enabled: false,
            mcp_elicitation_warn_sensitive: true,
            quality_self_check: false,
            quality_trigger: "has_retrieval".to_owned(),
            quality_latency_budget_ms: 4_000,
            context_strategy: "full_history".to_owned(),
            mcp_discovery_strategy: "none".to_owned(),
            mcp_discovery_top_k: 10,
            mcp_discovery_provider: String::new(),
            database_url: None,
            shell_transactional: false,
            shell_auto_rollback: false,
            shell_checkpoints_enabled: false,
            shell_max_checkpoints: 20,
            file_deny_read: Vec::new(),
            file_allow_read: Vec::new(),
            sandbox_enabled: false,
            sandbox_profile: "workspace".to_owned(),
            sandbox_backend: zeph_config::SandboxBackend::Auto,
            sandbox_strict: true,
            sandbox_allow_read: Vec::new(),
            sandbox_allow_write: Vec::new(),
            sandbox_denied_domains: Vec::new(),
            sandbox_fail_if_unavailable: false,
            budget_hint_enabled: true,
            time_reminder_enabled: false,
            time_reminder_interval_requests: 10,
            forgetting_enabled: false,
            microcompact_enabled: false,
            microcompact_gap_threshold_minutes: 60,
            autodream_enabled: false,
            autodream_min_sessions: 5,
            autodream_min_hours: 8,
            magic_docs_enabled: false,
            telemetry_enabled: false,
            prometheus_enabled: false,
            trajectory_critical_at: 10.0,
            trajectory_auto_recover: 16,
            gonka_private_key: None,
            gonka_address: None,
            gonka_nodes: Vec::new(),
            cocoon_client_url: None,
            cocoon_wants_access_hash: false,
            cocoon_show_balance: true,
            fidelity_enabled: false,
            type_aware_compose_enabled: false,
            type_aware_compose_intent_scoped: false,
            worktree_enabled: false,
            worktree_bg_isolation: BgIsolation::Worktree,
            worktree_base_ref: WorktreeBaseRef::Head,
            worktree_max_worktrees: None,
            worktree_disk_quota_mb: None,
            worktree_auto_reconcile_secs: 0,
            durable: zeph_core::config::DurableConfig::default(),
            durable_key_b64: None,
            caveman_default_on: false,
            knowledge_ingest_provider: String::new(),
            #[cfg(feature = "deep-link")]
            deep_link_register: false,
            #[cfg(feature = "deep-link")]
            deep_link_confirm_before_prompt: true,
            tui_theme_name: "zephyr".to_owned(),
            tui_color_mode: zeph_config::ColorMode::Auto,
            tui_delights_enabled: true,
            tui_mouse_enabled: false,
            session_persistence_enabled: zeph_config::SessionConfig::default().enabled,
            session_data_dir: zeph_config::SessionConfig::default().data_dir,
            serve_http_addr: zeph_config::ServeConfig::default().http_addr,
            serve_require_auth: zeph_config::ServeConfig::default().require_auth,
            serve_auth_token_vault_key: zeph_config::ServeConfig::default().auth_token_vault_key,
            serve_max_sessions: zeph_config::ServeConfig::default().max_sessions,
            serve_session_idle_ttl_secs: zeph_config::ServeConfig::default().session_idle_ttl_secs,
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
    step_integrity(&mut state)?;
    step_llm(&mut state)?;
    step_memory(&mut state)?;
    step_context_compression(&mut state)?;
    step_channel(&mut state)?;
    step_update_check(&mut state)?;
    step_scheduler(&mut state)?;
    step_skills_registry(&mut state)?;
    step_search(&mut state)?;
    step_orchestration(&mut state)?;
    step_durable(&mut state)?;
    step_daemon(&mut state)?;
    step_acp(&mut state)?;
    step_mcpls(&mut state)?;
    step_mcp_remote(&mut state)?;
    step_mcp_discovery(&mut state)?;
    step_lsp_context(&mut state)?;
    step_agents(&mut state)?;
    step_worktree(&mut state)?;
    step_router(&mut state)?;
    step_learning(&mut state)?;
    step_security(&mut state)?;
    step_sandbox(&mut state)?;
    step_debug(&mut state)?;
    step_logging(&mut state)?;
    step_experiments(&mut state)?;
    step_retry(&mut state)?;
    step_policy(&mut state)?;
    step_trajectory(&mut state)?;
    step_telemetry(&mut state)?;
    step_prometheus(&mut state)?;
    step_session(&mut state)?;
    step_serve(&mut state)?;
    step_session_recap(&mut state)?;
    step_plugins_reputation(&mut state)?;
    step_caveman(&mut state)?;
    step_knowledge(&mut state)?;
    #[cfg(feature = "deep-link")]
    step_deep_link(&mut state)?;
    step_tui_theme(&mut state)?;
    step_tui_delights(&mut state)?;
    step_tui_mouse(&mut state)?;
    step_quality(&mut state)?;
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
            let interval_ms: u64 = Input::new()
                .with_prompt(
                    "Streaming edit interval in ms (rate-limit safe: >=2000, minimum enforced: 500)",
                )
                .default(3000u64)
                .interact_text()?;
            state.telegram_stream_interval_ms = interval_ms;
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
            let user_ids: String = Input::new()
                .with_prompt(
                    "Allowed Discord user IDs (comma-separated; required unless a role is set)",
                )
                .default(String::new())
                .interact_text()?;
            state.discord_allowed_user_ids = user_ids
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            let role_ids: String = Input::new()
                .with_prompt("Allowed Discord role IDs (comma-separated, optional)")
                .default(String::new())
                .interact_text()?;
            state.discord_allowed_role_ids = role_ids
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
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
            let user_ids: String = Input::new()
                .with_prompt("Allowed Slack user IDs (comma-separated)")
                .default(String::new())
                .interact_text()?;
            state.slack_allowed_user_ids = user_ids
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
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

    let backends = [
        "age (encrypted file, recommended)",
        "env (environment variables)",
    ];
    let selection = Select::new()
        .with_prompt("Select secrets backend")
        .items(backends)
        .default(0)
        .interact()?;

    state.vault_backend = match selection {
        0 => "age".into(),
        1 => "env".into(),
        _ => unreachable!(),
    };

    println!();
    Ok(())
}

/// Offer to generate and store `ZEPH_HISTORY_KEY` (issue #6449), the root secret that activates
/// transcript/session hash-chain tamper-evidence (#6360) and vault-anchor downgrade-resistance
/// (#6449, `[integrity] anchor = "vault"`, the default). Without this secret, `anchor = "vault"`
/// stays inert — chained files verify their internal consistency but nothing detects a whole-file
/// strip.
fn step_integrity(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 1b/10: Transcript/Session Tamper-Evidence ==\n");
    println!(
        "Zeph can cryptographically anchor sub-agent transcripts and session logs against \
         tampering and downgrade attacks (issues #6360/#6449). This requires a \
         ZEPH_HISTORY_KEY secret in the vault.\n"
    );

    if state.vault_backend != "age" {
        println!(
            "Secrets backend is \"{}\", not \"age\" — history tamper-anchoring requires the age \
             vault and will stay inactive until you switch backends and provision \
             ZEPH_HISTORY_KEY manually.\n",
            state.vault_backend
        );
        return Ok(());
    }

    let generate = Confirm::new()
        .with_prompt("Generate and store a ZEPH_HISTORY_KEY now? (recommended)")
        .default(true)
        .interact()?;

    if generate {
        let dir = zeph_core::vault::default_vault_dir();
        if !dir.join("vault-key.txt").exists() {
            zeph_core::vault::AgeVaultProvider::init_vault(&dir)?;
        }
        let mut provider = zeph_core::vault::AgeVaultProvider::load(
            &dir.join("vault-key.txt"),
            &dir.join("secrets.age"),
        )?;
        if provider.get("ZEPH_HISTORY_KEY").is_none() {
            let key = zeph_core::history_integrity::generate_history_key_b64();
            provider.set_secret_mut("ZEPH_HISTORY_KEY".to_owned(), key, false)?;
            provider.save()?;
            println!("ZEPH_HISTORY_KEY generated and stored in the vault.\n");
        } else {
            println!("ZEPH_HISTORY_KEY already present in the vault — left unchanged.\n");
        }
    } else {
        println!(
            "Skipped. Default `[integrity] anchor = \"vault\"` will stay inactive (chain-only \
             #6453-level protection; `zeph doctor` will show a WARN) until ZEPH_HISTORY_KEY is \
             provisioned — generate one later with `zeph vault set ZEPH_HISTORY_KEY <base64>`.\n"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(crate) fn build_config(state: &WizardState) -> Config {
    let mut config = Config::default();
    config.agent.auto_update_check = state.auto_update_check;
    config.agent.budget_hint_enabled = state.budget_hint_enabled;
    config.agent.time_reminder_enabled = state.time_reminder_enabled;
    config.agent.time_reminder_interval_requests = state.time_reminder_interval_requests;
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
            embedding_model: if llm::supports_embeddings(provider.as_str()) {
                state.embedding_model.clone()
            } else {
                None
            },
            thinking: state.thinking.clone(),
            server_compaction: state.server_compaction_enabled,
            enable_extended_context: state.enable_extended_context,
            reasoning_effort: state.reasoning_effort.clone(),
            thinking_level: state.gemini_thinking_level,
            vision_model: state.vision_model.clone().filter(|s| !s.is_empty()),
            gonka_nodes: state.gonka_nodes.clone(),
            cocoon_client_url: state.cocoon_client_url.clone(),
            cocoon_access_hash: if state.cocoon_wants_access_hash {
                Some(String::new()) // empty sentinel: resolve from vault
            } else {
                None
            },
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
        embedding_model: if llm::supports_embeddings(provider.as_str()) {
            state
                .embedding_model
                .clone()
                .unwrap_or_else(|| "qwen3-embedding".into())
        } else {
            String::new()
        },
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
        stream_limits: zeph_config::StreamLimits::default(),
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
        database_url: state
            .database_url
            .clone()
            .map(zeph_common::secret::Secret::new),
        // Never write the Qdrant API key to config — it lives in the vault only.
        // The vault key ZEPH_QDRANT_API_KEY is resolved at runtime by resolve_secrets().
        qdrant_api_key: None,
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
        config.memory.compression.probe.probe_provider =
            Some(zeph_config::ProviderName::new(p.clone()));
    }
    if state.probe_enabled {
        config.memory.compression.probe.threshold = state.probe_threshold;
        config.memory.compression.probe.hard_fail_threshold = state.probe_hard_fail_threshold;
    }
    config.memory.shutdown_summary = state.shutdown_summary;
    config.memory.digest.enabled = state.digest_enabled;
    config.memory.store.enabled = state.store_enabled;
    config.memory.store.max_value_bytes = state.store_max_value_bytes;
    config.session.recap.on_resume = state.recap_on_resume;
    config.session.resume.show_banner = state.resume_show_banner;
    config.plugins.reputation.enabled = state.plugins_reputation_enabled;
    config.session.persist_provider_overrides = state.persist_provider_overrides;
    config.session.enabled = state.session_persistence_enabled;
    config.session.data_dir.clone_from(&state.session_data_dir);
    config.serve.http_addr.clone_from(&state.serve_http_addr);
    config.serve.require_auth = state.serve_require_auth;
    config
        .serve
        .auth_token_vault_key
        .clone_from(&state.serve_auth_token_vault_key);
    config.serve.max_sessions = state.serve_max_sessions;
    config.serve.session_idle_ttl_secs = state.serve_session_idle_ttl_secs;
    config.caveman.default_on = state.caveman_default_on;
    config
        .knowledge
        .ingest_provider
        .clone_from(&state.knowledge_ingest_provider);
    config.cocoon.show_balance = state.cocoon_show_balance;
    config.mcp.elicitation_enabled = state.mcp_elicitation_enabled;
    config.mcp.elicitation_warn_sensitive_fields = state.mcp_elicitation_warn_sensitive;
    config.quality.self_check = state.quality_self_check;
    config.quality.trigger = match state.quality_trigger.as_str() {
        "always" => TriggerPolicy::Always,
        "manual" => TriggerPolicy::Manual,
        _ => TriggerPolicy::HasRetrieval,
    };
    config.quality.latency_budget_ms = state.quality_latency_budget_ms;
    config.memory.context_strategy = match state.context_strategy.as_str() {
        "memory_first" => zeph_core::config::ContextStrategy::MemoryFirst,
        "adaptive" => zeph_core::config::ContextStrategy::Adaptive,
        _ => zeph_core::config::ContextStrategy::FullHistory,
    };

    if state.fidelity_enabled {
        config.memory.fidelity = Some(zeph_config::FidelityConfig {
            enabled: true,
            ..Default::default()
        });
    }

    config.memory.type_aware_compose.enabled = state.type_aware_compose_enabled;
    config.memory.type_aware_compose.intent_scoped =
        state.type_aware_compose_enabled && state.type_aware_compose_intent_scoped;

    // MM-F1/F2/F5 retrieval tuning defaults — no interactive question needed;
    // all fields have sensible defaults. Surfaced here per CLAUDE.md rule #4.
    println!(
        "  retrieval: depth={} template={} context_format={:?}",
        if config.memory.retrieval.depth == 0 {
            "legacy (limit*2)".to_owned()
        } else {
            config.memory.retrieval.depth.to_string()
        },
        if config.memory.retrieval.search_prompt_template.is_empty() {
            "none"
        } else {
            "custom"
        },
        config.memory.retrieval.context_format,
    );
    println!("  (edit config.toml to override [memory.retrieval])");

    match state.channel {
        ChannelChoice::Cli => {}
        ChannelChoice::Telegram => {
            config.telegram = Some(TelegramConfig {
                token: None,
                allowed_users: state.telegram_users.clone(),
                skills: ChannelSkillsConfig::default(),
                allowed_tools: None,
                stream_interval_ms: state.telegram_stream_interval_ms,
                guest_mode: false,
                bot_to_bot: false,
                allowed_bots: vec![],
                max_bot_chain_depth: 3,
            });
        }
        ChannelChoice::Discord => {
            config.discord = Some(DiscordConfig {
                token: None,
                application_id: state.discord_app_id.clone(),
                allowed_user_ids: state.discord_allowed_user_ids.clone(),
                allowed_role_ids: state.discord_allowed_role_ids.clone(),
                allowed_channel_ids: vec![],
                skills: ChannelSkillsConfig::default(),
                allowed_tools: None,
            });
        }
        ChannelChoice::Slack => {
            config.slack = Some(SlackConfig {
                bot_token: None,
                signing_secret: None,
                webhook_host: "127.0.0.1".into(),
                port: 3000,
                allowed_user_ids: state.slack_allowed_user_ids.clone(),
                allowed_channel_ids: vec![],
                skills: ChannelSkillsConfig::default(),
                allowed_tools: None,
            });
        }
    }

    config.vault = VaultConfig {
        backend: match state.vault_backend.as_str() {
            "env" => VaultBackend::Env,
            "keyring" => VaultBackend::Keyring,
            // Defensive fallback (state.vault_backend is always "age" or "env" once
            // step_vault runs): prefer the stronger, spec-010-recommended backend.
            _ => VaultBackend::Age,
        },
    };

    apply_daemon_config(&mut config, state);
    apply_acp_config(&mut config, state);

    config.scheduler = zeph_core::config::SchedulerConfig {
        enabled: state.scheduler_enabled,
        tick_interval_secs: state.scheduler_tick_interval_secs,
        max_tasks: state.scheduler_max_tasks,
        tasks: Vec::new(),
        daemon: zeph_core::config::SchedulerDaemonConfig::default(),
        security: SchedulerSecurityConfig::default(),
    };

    config.skills.registry = zeph_core::config::RegistryConfig {
        enabled: state.skills_registry_enabled,
        ..zeph_core::config::RegistryConfig::default()
    };

    config.tools.search = zeph_tools::SearchConfig {
        enabled: state.search_enabled,
        ..zeph_tools::SearchConfig::default()
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
    config.agents.forward_transcript = state.agents_forward_transcript;

    // Worktree isolation for sub-agents (#4656)
    if state.worktree_enabled {
        config.worktree.enabled = true;
        config.worktree.bg_isolation = state.worktree_bg_isolation;
        config.worktree.base_ref = state.worktree_base_ref.clone();
        // Worktree disk-quota + auto-reconcile (#5924)
        config.worktree.max_worktrees = state.worktree_max_worktrees;
        config.worktree.disk_quota_mb = state.worktree_disk_quota_mb;
        config.worktree.auto_reconcile_secs = state.worktree_auto_reconcile_secs;
    }

    // Durable execution layer (spec-064, #4949). The AEAD key lives only in the vault.
    config.durable = state.durable.clone();

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
                config.skills.learning.feedback_provider = ProviderName::new(provider.as_str());
            }
        }
        _ => {}
    }

    config.orchestration = OrchestrationConfig {
        enabled: state.orchestration_enabled,
        max_tasks: state.orchestration_max_tasks,
        max_parallel: state.orchestration_max_parallel,
        confirm_before_execute: state.orchestration_confirm_before_execute,
        default_failure_strategy: state
            .orchestration_failure_strategy
            .parse::<zeph_config::FailureStrategy>()
            .unwrap_or_default(),
        planner_provider: ProviderName::new(
            state
                .orchestration_planner_provider
                .clone()
                .unwrap_or_default(),
        ),
        persistence_enabled: state.orchestration_persistence_enabled,
        default_idle_timeout_secs: state.orchestration_default_idle_timeout_secs,
        ensemble: zeph_config::EnsembleConfig {
            enabled: state.ensemble_enabled,
            verify: state.ensemble_enabled,
            members: state.ensemble_members.clone(),
            ..zeph_config::EnsembleConfig::default()
        },
        command: zeph_config::CommandConfig {
            enabled: state.command_enabled,
            max_handoffs: state.command_max_handoffs,
        },
        ..OrchestrationConfig::default()
    };

    config.debug.enabled = state.debug_dump_enabled;
    config.debug.format = state.debug_dump_format;

    config.security.pii_filter.enabled = state.pii_filter_enabled;
    config.security.rate_limit.enabled = state.rate_limit_enabled;
    config.cost.max_daily_cents = state.max_daily_cents;
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
    config.tools.shell.checkpoints_enabled = state.shell_checkpoints_enabled;
    config.tools.shell.max_checkpoints = state.shell_max_checkpoints;
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
        "read-only" => zeph_config::tools::SandboxProfile::ReadOnly,
        "network-allow-all" => zeph_config::tools::SandboxProfile::NetworkAllowAll,
        "off" => zeph_config::tools::SandboxProfile::Off,
        other => {
            tracing::warn!(
                "unknown sandbox_profile value {:?}; defaulting to Workspace",
                other
            );
            zeph_config::tools::SandboxProfile::Workspace
        }
    };
    config.tools.sandbox.backend = state.sandbox_backend.clone();
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
    // Sandbox egress filter (#3294).
    config
        .tools
        .sandbox
        .denied_domains
        .clone_from(&state.sandbox_denied_domains);
    config.tools.sandbox.fail_if_unavailable = state.sandbox_fail_if_unavailable;
    config.skills.trust.scan_on_load = state.skill_scan_on_load;
    config.skills.trust.require_integrity_check_on_promote =
        state.skill_require_integrity_check_on_promote;
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
    if state.nli_enabled {
        config.security.content_isolation.nli.enabled = true;
        config.security.content_isolation.nli.provider =
            zeph_config::ProviderName::new(state.nli_provider.as_str());
    }
    if state.secret_masking_enabled {
        config.security.content_isolation.secret_masking.enabled = true;
    }
    {
        config.tools.policy.enabled = state.policy_enforcer_enabled;
        if !state.policy_provider.is_empty() {
            config.tools.policy.policy_provider =
                zeph_config::ProviderName::new(state.policy_provider.as_str());
        }
    }
    if state.utility_window > 0 {
        config.tools.utility.utility_window = state.utility_window;
    }
    config.security.trajectory.critical_at = state.trajectory_critical_at;
    config.security.trajectory.auto_recover_after_turns = state.trajectory_auto_recover;

    #[cfg(feature = "classifiers")]
    {
        config.classifiers.enabled = state.classifiers_enabled;
        config.classifiers.pii_enabled = state.pii_enabled;
    }

    config.tools.retry.max_attempts = state.retry_max_attempts;
    config.tools.retry.parameter_reformat_provider =
        zeph_config::ProviderName::new(state.retry_parameter_reformat_provider.as_str());

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
            policy: zeph_config::McpPolicy::default(),
            trust_level: McpTrustLevel::Trusted,
            tool_allowlist: None,
            allow_untrusted_without_allowlist: false,
            expected_tools: Vec::new(),
            roots: Vec::new(),
            tool_metadata: std::collections::HashMap::new(),
            elicitation_enabled: None,
            env_isolation: None,
            media_passthrough: false,
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
            ProviderName::new(state.mcp_discovery_provider.as_str());
    }

    if state.experiments_enabled {
        config.experiments.enabled = true;
        config.experiments.eval_provider =
            ProviderName::new(state.experiments_eval_provider.as_str());
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

    // Apply deep-link security settings (spec-066, TASK-9).
    #[cfg(feature = "deep-link")]
    {
        config.deep_link.confirm_before_prompt = state.deep_link_confirm_before_prompt;
    }

    // Apply TUI theme (#5087).
    config.tui.theme.name.clone_from(&state.tui_theme_name);
    config.tui.theme.color_mode = state.tui_color_mode;

    // Apply TUI delights (#5104).
    if !state.tui_delights_enabled {
        config.tui.delights = zeph_config::DelightsConfig {
            stream_metrics: false,
            toasts: false,
            completion_flash: false,
            smooth_scroll: false,
            splash_shimmer: false,
        };
    }

    // Apply TUI mouse mode (#5103).
    config.tui.mouse = state.tui_mouse_enabled;

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
        let additional_directories: Vec<zeph_config::AdditionalDir> = state
            .acp_additional_directories
            .iter()
            .filter_map(|p| {
                zeph_config::AdditionalDir::parse(p)
                    .map_err(|e| {
                        eprintln!(
                            "Warning: skipping invalid ACP directory {}: {e}",
                            p.display()
                        );
                    })
                    .ok()
            })
            .collect();
        config.acp = AcpConfig {
            enabled: true,
            agent_name: state.acp_agent_name.clone(),
            agent_version: state.acp_agent_version.clone(),
            additional_directories,
            auth_clients: state.acp_auth_clients.clone(),
            auth_methods: state.acp_auth_methods.clone(),
            message_ids_enabled: state.acp_message_ids_enabled,
            subagents: zeph_config::AcpSubagentsConfig {
                enabled: state.acp_subagents_enabled,
                ..zeph_config::AcpSubagentsConfig::default()
            },
            model_config: zeph_config::AcpModelConfigConfig {
                default_temperature_preset: state.acp_default_temperature_preset,
            },
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

    state.time_reminder_enabled = Confirm::new()
        .with_prompt(
            "Periodically remind the LLM of the current UTC time during long sessions? (time_reminder_enabled)",
        )
        .default(false)
        .interact()?;
    if state.time_reminder_enabled {
        state.time_reminder_interval_requests = Input::new()
            .with_prompt("Reminder interval, in agent turns (time_reminder_interval_requests)")
            .default(10u32)
            .interact_text()?;
    }

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
fn step_skills_registry(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Skill/Plugin Registry ==\n");

    state.skills_registry_enabled = Confirm::new()
        .with_prompt(
            "Enable external skill/plugin registry search (`zeph skill search`/`add`, \
             `zeph plugin search`/`get`)? Opt-in only — off by default, no network call is \
             ever made unless enabled.",
        )
        .default(false)
        .interact()?;

    if state.skills_registry_enabled {
        println!(
            "  Registry enabled with the default backend (skills.sh). If the backend \
             requires an auth token, store it with:\n    \
             zeph vault set ZEPH_SKILL_REGISTRY_TOKEN <token>\n  \
             then set skills.registry.auth_vault_key = \"ZEPH_SKILL_REGISTRY_TOKEN\" in \
             config.toml. This wizard never prompts for the raw token."
        );
    }

    println!();
    Ok(())
}

fn step_search(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Web Search Tool ==\n");

    state.search_enabled = Confirm::new()
        .with_prompt(
            "Enable the native `web_search` tool (query-based web search via an external \
             search API)? Opt-in only — off by default, disabled until an API key is stored.",
        )
        .default(false)
        .interact()?;

    if state.search_enabled {
        println!(
            "  Search enabled with the default backend (Brave Search API). Store the API key \
             with:\n    zeph vault set ZEPH_WEB_SEARCH_API_KEY <key>\n  \
             This wizard never prompts for the raw key. Override the backend, endpoint, or \
             vault key name under `[tools.search]` in config.toml."
        );
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

        let dirs_input: String = Input::new()
            .with_prompt(
                "Allowlisted additional directories for ACP sessions \
                 (comma-separated paths; empty = none)",
            )
            .default(String::new())
            .allow_empty(true)
            .interact_text()?;
        state.acp_additional_directories = dirs_input
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .collect();

        // Named bearer-token clients (#5868) — enables genuine multi-tenant/multi-window
        // isolation of persisted ACP sessions over HTTP/WS. Skippable: single-token or
        // stdio-only deployments need no entries here.
        let auth_clients_input: String = Input::new()
            .with_prompt(
                "Named bearer-token clients for HTTP/WS multi-tenant isolation \
                 (comma-separated \"id:token\" pairs; empty = none, use [acp] auth_token instead)",
            )
            .default(String::new())
            .allow_empty(true)
            .interact_text()?;
        state.acp_auth_clients = auth_clients_input
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(|pair| match pair.split_once(':') {
                Some((id, token)) if !id.is_empty() && !token.is_empty() => {
                    Some(zeph_config::AcpAuthClient {
                        id: id.to_owned(),
                        token: Some(token.to_owned()),
                        token_vault_key: None,
                    })
                }
                _ => {
                    eprintln!("Warning: skipping malformed auth client entry {pair:?} (expected \"id:token\")");
                    None
                }
            })
            .collect();

        // PR 4 MVP: only "agent" is offered; kept as a prompt for discoverability.
        state.acp_auth_methods = vec![zeph_config::AcpAuthMethod::Agent];

        state.acp_message_ids_enabled = Confirm::new()
            .with_prompt("Echo PromptRequest.message_id in responses/chunks (IDE correlation)?")
            .default(true)
            .interact()?;

        state.acp_subagents_enabled = Confirm::new()
            .with_prompt(
                "Enable ACP sub-agent delegation? \
                 (allows `zeph acp run-agent` to spawn child ACP agents)",
            )
            .default(false)
            .interact()?;

        let temperature_presets = [
            zeph_config::AcpTemperaturePreset::Precise,
            zeph_config::AcpTemperaturePreset::Balanced,
            zeph_config::AcpTemperaturePreset::Creative,
        ];
        let labels: Vec<String> = temperature_presets
            .iter()
            .map(|p| format!("{} (temperature {})", p.as_str(), p.temperature()))
            .collect();
        let default_idx = temperature_presets
            .iter()
            .position(|p| *p == state.acp_default_temperature_preset)
            .unwrap_or(1);
        let idx = Select::new()
            .with_prompt(
                "Default model_config sampling-temperature preset advertised to IDE clients",
            )
            .items(&labels)
            .default(default_idx)
            .interact()?;
        state.acp_default_temperature_preset = temperature_presets[idx];
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

fn provider_effective_name(state: &WizardState) -> String {
    match state.provider.unwrap_or(ProviderKind::Ollama) {
        ProviderKind::Compatible => state
            .compatible_name
            .clone()
            .unwrap_or_else(|| "compatible".into()),
        kind => kind.as_str().to_owned(),
    }
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
        let default_provider = provider_effective_name(state);
        let input: String = Input::new()
            .with_prompt(format!(
                "Provider name for evaluation judge (configured: {default_provider})"
            ))
            .default(default_provider.clone())
            .interact_text()?;
        state.experiments_eval_provider = input;

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
    println!(
        "Enabling this also enables the [gateway] HTTP listener if it is not already set. \
         The same listener additionally exposes a POST /webhook endpoint that forwards \
         external content directly into the agent's turn loop, as if from a trusted channel \
         (#6487).\n"
    );

    state.prometheus_enabled = Confirm::new()
        .with_prompt("Enable Prometheus metrics export?")
        .default(false)
        .interact()?;

    if state.prometheus_enabled {
        println!(
            "  The gateway refuses to start without a bearer token (#6487). Store one with:\n    \
             zeph vault set ZEPH_GATEWAY_TOKEN <token>\n  \
             This wizard never prompts for the raw token — see CLAUDE.md Secrets & Vault."
        );
    }

    println!();
    Ok(())
}

fn step_session_recap(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Session Recap & MCP Elicitation ==\n");
    state.recap_on_resume = Confirm::new()
        .with_prompt("Show a recap when resuming a conversation? [Y/n]")
        .default(true)
        .interact()?;

    state.resume_show_banner = Confirm::new()
        .with_prompt(
            "Show a \"Resuming session\" banner on CLI/TUI startup when history exists? [Y/n]",
        )
        .default(true)
        .interact()?;

    state.persist_provider_overrides = Confirm::new()
        .with_prompt(
            "Persist provider generation overrides (e.g. reasoning_effort) across restarts? [Y/n]",
        )
        .default(true)
        .interact()?;

    state.mcp_elicitation_enabled = Confirm::new()
        .with_prompt(
            "Allow MCP servers to request user input mid-task (elicitation)? [y/N]\n  \
             (opt-in; servers with elicitation can interrupt agent flow)",
        )
        .default(false)
        .interact()?;

    if state.mcp_elicitation_enabled {
        state.mcp_elicitation_warn_sensitive = Confirm::new()
            .with_prompt(
                "Warn before prompting for sensitive fields (password, token, etc.)? [Y/n]",
            )
            .default(true)
            .interact()?;
    }

    println!();
    Ok(())
}

fn step_plugins_reputation(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Plugin Install Safety ==\n");
    state.plugins_reputation_enabled = Confirm::new()
        .with_prompt(
            "Enable install-time typosquat check? [Y/n]\n  \
             (local, zero-network Levenshtein-similarity check against bundled/installed \
             names; advisory-only by default — warns, never blocks)",
        )
        .default(true)
        .interact()?;

    println!();
    Ok(())
}

fn step_caveman(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Output Style ==\n");
    state.caveman_default_on = Confirm::new()
        .with_prompt(
            "Enable caveman (ultra-terse, telegraphic) output by default? [y/N]\n  \
             (drops articles/filler; keeps code blocks and paths verbatim; toggleable at runtime with /caveman)",
        )
        .default(false)
        .interact()?;

    println!();
    Ok(())
}

fn step_quality(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Quality Self-Check (MARCH) ==\n");
    println!("Post-response Proposer+Checker pipeline that flags unsupported claims.");
    println!("Adds LLM latency/cost per turn; off by default.\n");

    state.quality_self_check = Confirm::new()
        .with_prompt("Enable post-response self-check?")
        .default(false)
        .interact()?;

    if state.quality_self_check {
        println!(
            "Note: dedicated proposer_provider / checker_provider can be set by editing \
             `proposer_provider` / `checker_provider` in config.toml; \
             both default to the primary provider when empty."
        );

        let triggers = [
            "has_retrieval (only when the turn used retrieval)",
            "always",
            "manual",
        ];
        let idx = Select::new()
            .with_prompt("When to trigger the pipeline")
            .items(triggers)
            .default(0)
            .interact()?;
        state.quality_trigger = match idx {
            1 => "always".into(),
            2 => "manual".into(),
            _ => "has_retrieval".into(),
        };

        state.quality_latency_budget_ms = Input::new()
            .with_prompt("Total pipeline latency budget (ms)")
            .default(4_000u64)
            .validate_with(|v: &u64| -> Result<(), &str> {
                if *v < 2_000 {
                    Err("must be >= 2000ms (one per-call timeout)")
                } else if *v > 60_000 {
                    Err("must be <= 60000ms")
                } else {
                    Ok(())
                }
            })
            .interact_text()?;
    }

    println!();
    Ok(())
}

fn step_knowledge(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Knowledge Ingest ==\n");
    println!(
        "Optionally configure a dedicated provider for knowledge ingest (Phase 2 graph extraction)."
    );
    println!("Leave blank to use the primary provider.\n");

    let provider: String = Input::new()
        .with_prompt("Provider name for knowledge ingest (blank = primary)")
        .default(String::new())
        .allow_empty(true)
        .interact_text()?;

    if !provider.is_empty() {
        state.knowledge_ingest_provider = provider;
    }

    println!();
    Ok(())
}

/// Configure `zeph://` deep-link URI scheme (spec-066, TASK-9).
///
/// Offers to register the OS-level `zeph://` handler and configures the security gate
/// (`confirm_before_prompt`). Gated behind the `deep-link` Cargo feature.
///
/// # Errors
///
/// Returns an error if the terminal prompt interaction fails.
#[cfg(feature = "deep-link")]
fn step_deep_link(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Deep Link (zeph:// URI scheme) ==\n");
    println!("Allows other applications to open a Zeph session via a zeph:// URL.");
    println!("Requires the binary to be compiled with --features deep-link.\n");

    state.deep_link_register = Confirm::new()
        .with_prompt("Register the zeph:// URI scheme on this OS?")
        .default(false)
        .interact()?;

    if state.deep_link_register {
        state.deep_link_confirm_before_prompt = Confirm::new()
            .with_prompt(
                "Require confirmation before injecting a URL-supplied prompt? (recommended) [Y/n]",
            )
            .default(true)
            .interact()?;
    }

    println!();
    Ok(())
}

/// Ask the user to choose a TUI colour theme and terminal colour mode.
///
/// Skipped when the `tui` feature is not compiled in (selecting a theme for a CLI-only build
/// is pointless). Always shows a list of built-in presets plus an "enter custom name" option.
///
/// # Errors
///
/// Returns an error if the terminal prompt interaction fails.
fn step_tui_delights(state: &mut WizardState) -> anyhow::Result<()> {
    use dialoguer::Confirm;
    println!("== TUI Micro-Delights ==\n");
    println!("Enable animated micro-delights in the TUI dashboard:");
    println!("  • tok/s and TTFT in the status bar during/after LLM turns");
    println!("  • ephemeral toast notifications (theme switch, copy, etc.)");
    println!("  • completion flash on finished tool groups");
    println!("  • smooth scroll on page jumps");
    println!("  • one-shot shimmer on the splash wordmark");
    println!();
    println!("All are controlled by [tui.delights] in config.toml.");
    println!("motion = off acts as a master kill-switch regardless.\n");

    let enabled = Confirm::new()
        .with_prompt("Enable TUI micro-delights?")
        .default(true)
        .interact()?;

    state.tui_delights_enabled = enabled;
    println!();
    Ok(())
}

fn step_tui_mouse(state: &mut WizardState) -> anyhow::Result<()> {
    use dialoguer::Confirm;
    println!("== TUI Mouse Mode ==\n");
    println!("Enable opt-in mouse capture in the TUI dashboard?");
    println!("  • Scroll wheel scrolls the transcript");
    println!("  • Left click focuses a panel");
    println!("  • Text selection still works via Shift+drag");
    println!("  • Enabled/disabled at runtime with /mouse on|off\n");

    let enabled = Confirm::new()
        .with_prompt("Enable TUI mouse capture at startup?")
        .default(false)
        .interact()?;

    state.tui_mouse_enabled = enabled;
    println!();
    Ok(())
}

fn step_tui_theme(state: &mut WizardState) -> anyhow::Result<()> {
    use dialoguer::Select;

    println!("== TUI Theme ==\n");
    println!("Choose the visual colour palette for the TUI dashboard (--tui).");
    println!("You can change this later in config.toml under [tui.theme].\n");

    let presets = [
        "zephyr (default — dark blue-green)",
        "classic (legacy look, matches pre-2.0 colours)",
        "zephyr-light (light variant)",
        "high-contrast",
        "catppuccin-mocha",
        "gruvbox-dark",
        "solarized-dark",
    ];
    let preset_names = [
        "zephyr",
        "classic",
        "zephyr-light",
        "high-contrast",
        "catppuccin-mocha",
        "gruvbox-dark",
        "solarized-dark",
    ];

    let idx = Select::new()
        .with_prompt("TUI theme")
        .items(presets)
        .default(0)
        .interact()?;

    preset_names[idx].clone_into(&mut state.tui_theme_name);

    let color_mode_labels = [
        "auto (detect terminal capability)",
        "truecolor (24-bit RGB — requires a modern terminal)",
        "ansi256 (256-colour fallback)",
        "ansi16 (basic 16 colours)",
        "never (disable colour / respect NO_COLOR)",
    ];
    let color_modes = [
        zeph_config::ColorMode::Auto,
        zeph_config::ColorMode::Truecolor,
        zeph_config::ColorMode::Ansi256,
        zeph_config::ColorMode::Ansi16,
        zeph_config::ColorMode::Never,
    ];

    let cm_idx = Select::new()
        .with_prompt("Terminal colour mode")
        .items(color_mode_labels)
        .default(0)
        .interact()?;

    state.tui_color_mode = color_modes[cm_idx];

    println!();
    Ok(())
}

/// Compute the XDG default config path used by the init wizard.
///
/// Mirrors the fallback logic in `resolve_config_path_impl` so that the path the wizard
/// proposes matches the path the runtime loads when no CLI override or `ZEPH_CONFIG` env var
/// is set and `config/default.toml` does not exist.
pub(crate) fn wizard_default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map_or_else(|_| PathBuf::from("~"), PathBuf::from)
                .join(".config")
        })
        .join("zeph")
        .join("config.toml")
}

fn step_review_and_write(state: &WizardState, output: Option<PathBuf>) -> anyhow::Result<()> {
    println!("== Step 10/10: Review & Write ==\n");

    let config = build_config(state);
    let toml_str = toml::to_string_pretty(&config)?;

    println!("--- Generated config ---");
    println!("{toml_str}");
    println!("------------------------\n");

    let default_path = wizard_default_config_path();
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

    durable::store_durable_key(state)?;
    print_secrets_instructions(state);
    print_next_steps(state, &path);

    // Perform OS-level scheme registration after writing the config (TASK-9).
    #[cfg(feature = "deep-link")]
    if state.deep_link_register {
        println!("\nRegistering zeph:// URI scheme...");
        match crate::url_scheme::register::handle_url_scheme_register() {
            Ok(()) => println!("zeph:// URI scheme registered."),
            Err(e) => eprintln!("Warning: scheme registration failed: {e}"),
        }
    }

    Ok(())
}

fn api_key_env_var(kind: ProviderKind, name: Option<&str>) -> Option<String> {
    match kind {
        ProviderKind::Claude => Some("ZEPH_CLAUDE_API_KEY".to_owned()),
        ProviderKind::OpenAi => Some("ZEPH_OPENAI_API_KEY".to_owned()),
        ProviderKind::Gemini => Some("ZEPH_GEMINI_API_KEY".to_owned()),
        ProviderKind::Compatible => {
            let n: String = name
                .unwrap_or("custom")
                .to_uppercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
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

    // Gonka native provider: private key and address are stored in vault (never in config file).
    if state.provider == Some(ProviderKind::Gonka) && state.gonka_private_key.is_some() {
        secrets.push("ZEPH_GONKA_PRIVATE_KEY".into());
        secrets.push("ZEPH_GONKA_ADDRESS".into());
    }

    // Cocoon provider: access hash is stored in vault (never in config file).
    if state.provider == Some(ProviderKind::Cocoon) && state.cocoon_wants_access_hash {
        secrets.push("ZEPH_COCOON_ACCESS_HASH".into());
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
    println!("Tip: run `zeph doctor` to verify provider connectivity and configuration health.");
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
    fn build_config_type_aware_compose_disabled_by_default() {
        let state = single_provider_state();
        let config = build_config(&state);
        assert!(!config.memory.type_aware_compose.enabled);
        assert!(!config.memory.type_aware_compose.intent_scoped);
    }

    #[test]
    fn build_config_type_aware_compose_enabled_wires_intent_scoped() {
        let state = WizardState {
            type_aware_compose_enabled: true,
            type_aware_compose_intent_scoped: true,
            ..single_provider_state()
        };
        let config = build_config(&state);
        assert!(config.memory.type_aware_compose.enabled);
        assert!(config.memory.type_aware_compose.intent_scoped);
    }

    #[test]
    fn build_config_type_aware_compose_intent_scoped_ignored_when_disabled() {
        // intent_scoped must not leak true when the master switch is off — the wizard only
        // asks the intent_scoped question when enabled is confirmed, but build_config must be
        // defensive against stale WizardState too.
        let state = WizardState {
            type_aware_compose_enabled: false,
            type_aware_compose_intent_scoped: true,
            ..single_provider_state()
        };
        let config = build_config(&state);
        assert!(!config.memory.type_aware_compose.enabled);
        assert!(!config.memory.type_aware_compose.intent_scoped);
    }

    #[test]
    fn build_config_store_disabled_by_default() {
        let state = single_provider_state();
        let config = build_config(&state);
        assert!(!config.memory.store.enabled);
    }

    #[test]
    fn build_config_plugins_reputation_enabled_by_default() {
        // spec-043 (#5864): the wizard defaults to enabling the advisory typosquat check.
        let state = single_provider_state();
        let config = build_config(&state);
        assert!(config.plugins.reputation.enabled);
    }

    #[test]
    fn build_config_plugins_reputation_disabled_when_declined() {
        let state = WizardState {
            plugins_reputation_enabled: false,
            ..single_provider_state()
        };
        let config = build_config(&state);
        assert!(!config.plugins.reputation.enabled);
    }

    #[test]
    fn build_config_store_enabled_wires_max_value_bytes() {
        // spec-080 (#6363) FR-A-010: the --init wizard's [memory.store] prompts must reach
        // the assembled Config unchanged.
        let state = WizardState {
            store_enabled: true,
            store_max_value_bytes: 131_072,
            ..single_provider_state()
        };
        let config = build_config(&state);
        assert!(config.memory.store.enabled);
        assert_eq!(config.memory.store.max_value_bytes, 131_072);
    }

    #[test]
    fn build_config_command_handoff_disabled_by_default() {
        let state = single_provider_state();
        let config = build_config(&state);
        assert!(!config.orchestration.command.enabled);
    }

    #[test]
    fn build_config_command_handoff_enabled_wires_max_handoffs() {
        // spec-080 (#6363) FR-B-014: the --init wizard's [orchestration.command] prompts
        // must reach the assembled Config unchanged.
        let state = WizardState {
            command_enabled: true,
            command_max_handoffs: 32,
            ..single_provider_state()
        };
        let config = build_config(&state);
        assert!(config.orchestration.command.enabled);
        assert_eq!(config.orchestration.command.max_handoffs, 32);
    }

    #[test]
    fn build_config_mcp_remote_server_media_passthrough_round_trips() {
        // spec-072 P3: the wizard's per-server image-passthrough prompt result must reach
        // `config.mcp.servers` unchanged (#6241).
        let state = WizardState {
            mcp_remote_servers: vec![McpServerConfig {
                id: "vision-tool".to_owned(),
                command: None,
                args: Vec::new(),
                env: std::collections::HashMap::new(),
                url: Some("https://mcp.example.com".to_owned()),
                timeout: 30,
                policy: zeph_config::McpPolicy::default(),
                headers: std::collections::HashMap::new(),
                oauth: None,
                trust_level: McpTrustLevel::Untrusted,
                tool_allowlist: None,
                allow_untrusted_without_allowlist: false,
                expected_tools: Vec::new(),
                roots: Vec::new(),
                tool_metadata: std::collections::HashMap::new(),
                elicitation_enabled: None,
                env_isolation: None,
                media_passthrough: true,
            }],
            ..single_provider_state()
        };
        let config = build_config(&state);
        let server = config
            .mcp
            .servers
            .iter()
            .find(|s| s.id == "vision-tool")
            .expect("server must be present in config.mcp.servers");
        assert!(
            server.media_passthrough,
            "media_passthrough=true must round-trip from WizardState to Config"
        );
    }

    #[test]
    fn build_config_mcp_remote_server_media_passthrough_defaults_false() {
        let state = WizardState {
            mcp_remote_servers: vec![McpServerConfig {
                id: "text-tool".to_owned(),
                command: None,
                args: Vec::new(),
                env: std::collections::HashMap::new(),
                url: Some("https://mcp.example.com".to_owned()),
                timeout: 30,
                policy: zeph_config::McpPolicy::default(),
                headers: std::collections::HashMap::new(),
                oauth: None,
                trust_level: McpTrustLevel::Untrusted,
                tool_allowlist: None,
                allow_untrusted_without_allowlist: false,
                expected_tools: Vec::new(),
                roots: Vec::new(),
                tool_metadata: std::collections::HashMap::new(),
                elicitation_enabled: None,
                env_isolation: None,
                media_passthrough: false,
            }],
            ..single_provider_state()
        };
        let config = build_config(&state);
        let server = config
            .mcp
            .servers
            .iter()
            .find(|s| s.id == "text-tool")
            .expect("server must be present in config.mcp.servers");
        assert!(!server.media_passthrough, "default-No must stay false");
    }

    #[test]
    fn build_config_claude_skips_embedding_model() {
        let state = WizardState {
            provider: Some(ProviderKind::Claude),
            model: Some("claude-sonnet-4-5-20250929".into()),
            embedding_model: Some("qwen3-embedding".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(
            config.llm.providers[0].embedding_model.is_none(),
            "Claude provider must not have embedding_model set"
        );
        assert!(
            config.llm.embedding_model.is_empty(),
            "llm.embedding_model must be empty for non-embedding providers"
        );
    }

    #[test]
    fn build_config_ollama_keeps_embedding_model() {
        let state = WizardState {
            provider: Some(ProviderKind::Ollama),
            model: Some("qwen3:8b".into()),
            embedding_model: Some("qwen3-embedding".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.llm.providers[0].embedding_model.as_deref(),
            Some("qwen3-embedding")
        );
        assert_eq!(config.llm.embedding_model, "qwen3-embedding");
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
    fn build_config_time_reminder_disabled_by_default() {
        let state = WizardState {
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(!config.agent.time_reminder_enabled);
    }

    #[test]
    fn build_config_time_reminder_enabled_wires_interval() {
        let state = WizardState {
            time_reminder_enabled: true,
            time_reminder_interval_requests: 5,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.agent.time_reminder_enabled);
        assert_eq!(config.agent.time_reminder_interval_requests, 5);
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
            experiments_eval_provider: "claude".into(),
            experiments_schedule_enabled: true,
            experiments_schedule_cron: "0 4 * * *".into(),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.experiments.enabled);
        assert_eq!(config.experiments.eval_provider.as_str(), "claude");
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

    #[test]
    fn provider_effective_name_claude() {
        let state = WizardState {
            provider: Some(ProviderKind::Claude),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        assert_eq!(provider_effective_name(&state), "claude");
    }

    #[test]
    fn provider_effective_name_compatible_with_name() {
        let state = WizardState {
            provider: Some(ProviderKind::Compatible),
            compatible_name: Some("my-provider".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        assert_eq!(provider_effective_name(&state), "my-provider");
    }

    #[test]
    fn provider_effective_name_compatible_without_name() {
        let state = WizardState {
            provider: Some(ProviderKind::Compatible),
            compatible_name: None,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        assert_eq!(provider_effective_name(&state), "compatible");
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
        assert_eq!(
            config
                .memory
                .compression
                .probe
                .probe_provider
                .as_ref()
                .map(ProviderName::as_str),
            Some("fast")
        );
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
        assert!(config.memory.compression.probe.probe_provider.is_none());
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
        let default_threshold = zeph_config::memory::CompactionProbeConfig::default().threshold;
        let default_hard_fail =
            zeph_config::memory::CompactionProbeConfig::default().hard_fail_threshold;
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
            config
                .memory
                .database_url
                .as_ref()
                .map(zeph_common::secret::Secret::expose),
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
            sandbox_backend: zeph_config::SandboxBackend::Auto,
            sandbox_strict: true,
            sandbox_allow_read: vec!["/tmp/read".into()],
            sandbox_allow_write: vec!["/tmp/write".into()],
            ..single_provider_state()
        };
        let config = build_config(&state);
        assert!(config.tools.sandbox.enabled);
        assert_eq!(
            config.tools.sandbox.profile,
            zeph_config::tools::SandboxProfile::Workspace
        );
        assert_eq!(
            config.tools.sandbox.backend,
            zeph_config::SandboxBackend::Auto
        );
        assert_eq!(config.tools.sandbox.allow_read.len(), 1);
        assert_eq!(config.tools.sandbox.allow_write.len(), 1);
    }

    #[test]
    fn build_config_sandbox_profile_variants() {
        for (input, expected) in [
            ("read-only", zeph_config::tools::SandboxProfile::ReadOnly),
            (
                "network-allow-all",
                zeph_config::tools::SandboxProfile::NetworkAllowAll,
            ),
            ("off", zeph_config::tools::SandboxProfile::Off),
            ("workspace", zeph_config::tools::SandboxProfile::Workspace),
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

    #[test]
    fn build_config_quality_disabled_by_default() {
        let state = WizardState {
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(!config.quality.self_check);
        assert_eq!(config.quality.trigger, TriggerPolicy::HasRetrieval);
    }

    #[test]
    fn build_config_quality_enabled_with_always() {
        let state = WizardState {
            quality_self_check: true,
            quality_trigger: "always".into(),
            quality_latency_budget_ms: 6_000,
            ..single_provider_state()
        };
        let config = build_config(&state);
        assert!(config.quality.self_check);
        assert_eq!(config.quality.trigger, TriggerPolicy::Always);
        assert_eq!(config.quality.latency_budget_ms, 6_000);
    }

    #[test]
    fn build_config_quality_trigger_manual() {
        let state = WizardState {
            quality_self_check: true,
            quality_trigger: "manual".into(),
            ..single_provider_state()
        };
        let config = build_config(&state);
        assert_eq!(config.quality.trigger, TriggerPolicy::Manual);
    }

    #[test]
    fn build_config_quality_trigger_unknown_falls_back_to_has_retrieval() {
        let state = WizardState {
            quality_self_check: true,
            quality_trigger: "unknown_value".into(),
            ..single_provider_state()
        };
        let config = build_config(&state);
        assert_eq!(config.quality.trigger, TriggerPolicy::HasRetrieval);
    }

    #[test]
    fn build_config_gonkagate_provider() {
        let state = WizardState {
            provider: Some(ProviderKind::Compatible),
            compatible_name: Some("gonkagate".into()),
            base_url: Some("https://api.gonkagate.com/v1".into()),
            model: Some("Qwen/Qwen3-235B-A22B-Instruct-2507-FP8".into()),
            embedding_model: Some("qwen3-embedding".into()),
            vault_backend: "age".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(config.llm.providers.len(), 1);
        let p = &config.llm.providers[0];
        assert_eq!(p.provider_type, ProviderKind::Compatible);
        assert_eq!(p.name.as_deref(), Some("gonkagate"));
        assert_eq!(p.base_url.as_deref(), Some("https://api.gonkagate.com/v1"));
        assert_eq!(
            p.model.as_deref(),
            Some("Qwen/Qwen3-235B-A22B-Instruct-2507-FP8")
        );
    }

    #[test]
    fn api_key_env_var_gonkagate() {
        assert_eq!(
            api_key_env_var(ProviderKind::Compatible, Some("gonkagate")),
            Some("ZEPH_COMPATIBLE_GONKAGATE_API_KEY".to_owned())
        );
    }

    #[test]
    fn api_key_env_var_hyphen_sanitized() {
        assert_eq!(
            api_key_env_var(ProviderKind::Compatible, Some("my-provider")),
            Some("ZEPH_COMPATIBLE_MY_PROVIDER_API_KEY".to_owned())
        );
    }

    #[test]
    fn collect_provider_secret_gonkagate_non_age_sets_key() {
        let mut secrets = Vec::new();
        collect_provider_secret(
            &mut secrets,
            Some(ProviderKind::Compatible),
            Some(&"gp-test-key".to_owned()),
            Some("gonkagate"),
            false,
        );
        assert_eq!(secrets, vec!["ZEPH_COMPATIBLE_GONKAGATE_API_KEY"]);
    }

    #[test]
    fn build_config_gonka_provider_snapshot() {
        let state = WizardState {
            provider: Some(ProviderKind::Gonka),
            model: Some("gpt-4o".into()),
            embedding_model: Some("text-embedding-3-small".into()),
            vault_backend: "age".into(),
            gonka_nodes: vec![
                zeph_config::GonkaNode {
                    url: "https://node1.gonka.ai".into(),
                    address: "gonka1node1placeholder000000000000000000000000".into(),
                    name: None,
                },
                zeph_config::GonkaNode {
                    url: "https://node2.gonka.ai".into(),
                    address: "gonka1node2placeholder000000000000000000000000".into(),
                    name: Some("backup".into()),
                },
            ],
            ..WizardState::default()
        };
        let config = build_config(&state);
        let providers = &config.llm.providers;
        assert_eq!(providers.len(), 1);
        let p = &providers[0];
        assert_eq!(p.provider_type, ProviderKind::Gonka);
        assert_eq!(p.model.as_deref(), Some("gpt-4o"));
        assert_eq!(p.gonka_nodes.len(), 2);
        assert_eq!(p.gonka_nodes[0].url, "https://node1.gonka.ai");
        assert_eq!(
            p.gonka_nodes[0].address,
            "gonka1node1placeholder000000000000000000000000"
        );
        assert_eq!(p.gonka_nodes[1].name.as_deref(), Some("backup"));
        // Snapshot the serialized TOML shape for regression detection.
        let toml_str = toml::to_string_pretty(p).expect("serialize provider entry");
        insta::assert_snapshot!("gonka_provider_entry_toml", toml_str);
    }

    // ── Cocoon wizard state → build_config ───────────────────────────────────

    #[test]
    fn build_config_cocoon_provider() {
        let state = WizardState {
            provider: Some(ProviderKind::Cocoon),
            model: Some("Qwen/Qwen3-0.6B".into()),
            cocoon_client_url: Some("http://localhost:10000".into()),
            cocoon_wants_access_hash: false,
            ..WizardState::default()
        };
        let config = build_config(&state);
        let p = &config.llm.providers[0];
        assert_eq!(p.provider_type, ProviderKind::Cocoon);
        assert_eq!(p.model.as_deref(), Some("Qwen/Qwen3-0.6B"));
        assert_eq!(
            p.cocoon_client_url.as_deref(),
            Some("http://localhost:10000")
        );
        assert!(p.cocoon_access_hash.is_none());
    }

    #[test]
    fn build_config_cocoon_access_hash_sentinel() {
        let state = WizardState {
            provider: Some(ProviderKind::Cocoon),
            model: Some("Qwen/Qwen3-0.6B".into()),
            cocoon_client_url: Some("http://localhost:10000".into()),
            cocoon_wants_access_hash: true,
            ..WizardState::default()
        };
        let config = build_config(&state);
        let p = &config.llm.providers[0];
        // Sentinel: Some("") signals "use vault".
        assert_eq!(p.cocoon_access_hash.as_deref(), Some(""));
    }

    #[test]
    fn build_config_cocoon_no_access_hash() {
        let state = WizardState {
            provider: Some(ProviderKind::Cocoon),
            model: Some("Qwen/Qwen3-0.6B".into()),
            cocoon_client_url: Some("http://localhost:10000".into()),
            cocoon_wants_access_hash: false,
            ..WizardState::default()
        };
        let config = build_config(&state);
        let p = &config.llm.providers[0];
        assert!(p.cocoon_access_hash.is_none());
    }

    #[test]
    fn build_config_worktree_disabled_by_default() {
        let state = WizardState::default();
        let config = build_config(&state);
        assert!(!config.worktree.enabled);
    }

    #[test]
    fn build_config_worktree_enabled_bg_isolation_none() {
        let state = WizardState {
            worktree_enabled: true,
            worktree_bg_isolation: BgIsolation::None,
            worktree_base_ref: WorktreeBaseRef::Fresh,
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.worktree.enabled);
        assert_eq!(config.worktree.bg_isolation, BgIsolation::None);
        assert!(matches!(config.worktree.base_ref, WorktreeBaseRef::Fresh));
    }

    /// Regression test for #5924: disabled state leaves the quota fields at their
    /// `WorktreeConfig::default()` values (unlimited / no accounting / sweep off).
    #[test]
    fn build_config_worktree_disabled_leaves_quota_fields_default() {
        let state = WizardState::default();
        let config = build_config(&state);
        assert_eq!(config.worktree.max_worktrees, None);
        assert_eq!(config.worktree.disk_quota_mb, None);
        assert_eq!(config.worktree.auto_reconcile_secs, 0);
    }

    #[test]
    fn build_config_worktree_enabled_with_quota_fields() {
        let state = WizardState {
            worktree_enabled: true,
            worktree_max_worktrees: Some(5),
            worktree_disk_quota_mb: Some(2048),
            worktree_auto_reconcile_secs: 3600,
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(config.worktree.max_worktrees, Some(5));
        assert_eq!(config.worktree.disk_quota_mb, Some(2048));
        assert_eq!(config.worktree.auto_reconcile_secs, 3600);
    }

    // ── #4981: api_key_env_var sanitization edge cases ──────────────────────

    #[test]
    fn api_key_env_var_dot_sanitized() {
        assert_eq!(
            api_key_env_var(ProviderKind::Compatible, Some("my.provider")),
            Some("ZEPH_COMPATIBLE_MY_PROVIDER_API_KEY".to_owned())
        );
    }

    #[test]
    fn api_key_env_var_space_sanitized() {
        assert_eq!(
            api_key_env_var(ProviderKind::Compatible, Some("my provider")),
            Some("ZEPH_COMPATIBLE_MY_PROVIDER_API_KEY".to_owned())
        );
    }

    #[test]
    fn api_key_env_var_at_sign_sanitized() {
        assert_eq!(
            api_key_env_var(ProviderKind::Compatible, Some("my@provider")),
            Some("ZEPH_COMPATIBLE_MY_PROVIDER_API_KEY".to_owned())
        );
    }

    #[test]
    fn api_key_env_var_none_name_uses_custom_fallback() {
        assert_eq!(
            api_key_env_var(ProviderKind::Compatible, None),
            Some("ZEPH_COMPATIBLE_CUSTOM_API_KEY".to_owned())
        );
    }

    // ── #4982: non-embedding providers skip embedding_model ─────────────────

    #[test]
    fn build_config_candle_skips_embedding_model() {
        let state = WizardState {
            provider: Some(ProviderKind::Candle),
            model: Some("llama3:8b".into()),
            embedding_model: Some("qwen3-embedding".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(
            config.llm.providers[0].embedding_model.is_none(),
            "Candle provider must not have embedding_model set"
        );
        assert!(
            config.llm.embedding_model.is_empty(),
            "llm.embedding_model must be empty for Candle"
        );
    }

    #[test]
    fn build_config_gonka_keeps_embedding_model() {
        let state = WizardState {
            provider: Some(ProviderKind::Gonka),
            model: Some("gpt-4o".into()),
            embedding_model: Some("qwen3-embedding".into()),
            vault_backend: "age".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.llm.providers[0].embedding_model.as_deref(),
            Some("qwen3-embedding"),
            "Gonka supports embeddings and must write embedding_model"
        );
        assert_eq!(config.llm.embedding_model, "qwen3-embedding");
    }

    #[test]
    fn build_config_cocoon_keeps_embedding_model() {
        let state = WizardState {
            provider: Some(ProviderKind::Cocoon),
            model: Some("Qwen/Qwen3-0.6B".into()),
            embedding_model: Some("qwen3-embedding".into()),
            cocoon_client_url: Some("http://localhost:10000".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.llm.providers[0].embedding_model.as_deref(),
            Some("qwen3-embedding"),
            "Cocoon supports embeddings and must write embedding_model"
        );
        assert_eq!(config.llm.embedding_model, "qwen3-embedding");
    }

    #[test]
    fn build_config_openai_keeps_embedding_model() {
        let state = WizardState {
            provider: Some(ProviderKind::OpenAi),
            model: Some("gpt-4o".into()),
            embedding_model: Some("text-embedding-3-small".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.llm.providers[0].embedding_model.as_deref(),
            Some("text-embedding-3-small")
        );
        assert_eq!(config.llm.embedding_model, "text-embedding-3-small");
    }

    #[test]
    fn build_config_openai_writes_reasoning_effort() {
        let state = WizardState {
            provider: Some(ProviderKind::OpenAi),
            model: Some("o3".into()),
            reasoning_effort: Some("high".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.llm.providers[0].reasoning_effort.as_deref(),
            Some("high")
        );
    }

    #[test]
    fn build_config_reasoning_effort_defaults_to_none_when_skipped() {
        let state = WizardState {
            provider: Some(ProviderKind::OpenAi),
            model: Some("gpt-4o".into()),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.llm.providers[0].reasoning_effort.is_none());
    }

    // ── #4983: provider_effective_name edge cases ────────────────────────────

    #[test]
    fn provider_effective_name_ollama() {
        let state = WizardState {
            provider: Some(ProviderKind::Ollama),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        assert_eq!(provider_effective_name(&state), "ollama");
    }

    #[test]
    fn provider_effective_name_openai() {
        let state = WizardState {
            provider: Some(ProviderKind::OpenAi),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        assert_eq!(provider_effective_name(&state), "openai");
    }

    #[test]
    fn provider_effective_name_none_defaults_to_ollama() {
        // provider = None → defaults to Ollama in provider_effective_name
        let state = WizardState {
            provider: None,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        assert_eq!(provider_effective_name(&state), "ollama");
    }

    #[test]
    fn build_config_policy_provider_set() {
        let state = WizardState {
            policy_enforcer_enabled: true,
            policy_provider: "my-llm".into(),
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(config.tools.policy.enabled);
        assert_eq!(config.tools.policy.policy_provider.as_str(), "my-llm");
    }

    #[test]
    fn build_config_utility_window_set() {
        let state = WizardState {
            utility_window: 5,
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(config.tools.utility.utility_window, 5);
    }

    #[test]
    fn build_config_defaults_match_session_and_serve_config_defaults() {
        // A wizard run with no interactive customization of the new #5343 steps should produce
        // a config identical to [session]/[serve]'s own Default impls.
        let state = WizardState {
            vault_backend: "env".into(),
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert_eq!(
            config.session.enabled,
            zeph_config::SessionConfig::default().enabled
        );
        assert_eq!(
            config.session.data_dir,
            zeph_config::SessionConfig::default().data_dir
        );
        assert_eq!(
            config.serve.http_addr,
            zeph_config::ServeConfig::default().http_addr
        );
        assert_eq!(
            config.serve.require_auth,
            zeph_config::ServeConfig::default().require_auth
        );
        assert_eq!(
            config.serve.max_sessions,
            zeph_config::ServeConfig::default().max_sessions
        );
    }

    #[test]
    fn build_config_applies_customized_session_and_serve_settings() {
        let state = WizardState {
            vault_backend: "env".into(),
            session_persistence_enabled: false,
            session_data_dir: "/custom/sessions".into(),
            serve_http_addr: "0.0.0.0:9000".into(),
            serve_require_auth: false,
            serve_auth_token_vault_key: "MY_TOKEN".into(),
            serve_max_sessions: 10,
            serve_session_idle_ttl_secs: 60,
            ..WizardState::default()
        };
        let config = build_config(&state);
        assert!(!config.session.enabled);
        assert_eq!(config.session.data_dir, "/custom/sessions");
        assert_eq!(config.serve.http_addr, "0.0.0.0:9000");
        assert!(!config.serve.require_auth);
        assert_eq!(config.serve.auth_token_vault_key, "MY_TOKEN");
        assert_eq!(config.serve.max_sessions, 10);
        assert_eq!(config.serve.session_idle_ttl_secs, 60);
    }
}
