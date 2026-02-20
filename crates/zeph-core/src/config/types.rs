use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use zeph_skills::TrustLevel;
use zeph_tools::{AutonomyLevel, ToolsConfig};

use crate::vault::Secret;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub agent: AgentConfig,
    pub llm: LlmConfig,
    pub skills: SkillsConfig,
    pub memory: MemoryConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackConfig>,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub a2a: A2aServerConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub index: IndexConfig,
    #[serde(default)]
    pub vault: VaultConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    #[serde(default)]
    pub cost: CostConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub tui: TuiConfig,
    #[serde(skip)]
    pub secrets: ResolvedSecrets,
}

fn default_max_tool_iterations() -> usize {
    10
}

fn default_auto_update_check() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AgentConfig {
    pub name: String,
    #[serde(default = "default_max_tool_iterations")]
    pub max_tool_iterations: usize,
    #[serde(default)]
    pub summary_model: Option<String>,
    #[serde(default = "default_auto_update_check")]
    pub auto_update_check: bool,
}

/// LLM provider backend selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Ollama,
    Claude,
    OpenAi,
    Candle,
    Orchestrator,
    Compatible,
    Router,
}

impl ProviderKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Claude => "claude",
            Self::OpenAi => "openai",
            Self::Candle => "candle",
            Self::Orchestrator => "orchestrator",
            Self::Compatible => "compatible",
            Self::Router => "router",
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct LlmConfig {
    pub provider: ProviderKind,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudLlmConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openai: Option<OpenAiConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candle: Option<CandleConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<OrchestratorConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatible: Option<Vec<CompatibleConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub router: Option<RouterConfig>,
    pub stt: Option<SttConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_model: Option<String>,
}

fn default_embedding_model() -> String {
    "qwen3-embedding".into()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SttConfig {
    #[serde(default = "default_stt_provider")]
    pub provider: String,
    #[serde(default = "default_stt_model")]
    pub model: String,
    #[serde(default = "default_stt_language")]
    pub language: String,
    #[serde(default)]
    pub base_url: Option<String>,
}

pub(crate) fn default_stt_provider() -> String {
    "whisper".into()
}

pub(crate) fn default_stt_model() -> String {
    "whisper-1".into()
}

pub(crate) fn default_stt_language() -> String {
    "auto".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CloudLlmConfig {
    pub model: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompatibleConfig {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    #[serde(default)]
    pub embedding_model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouterConfig {
    pub chain: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CandleConfig {
    #[serde(default = "default_candle_source")]
    pub source: String,
    #[serde(default)]
    pub local_path: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default = "default_chat_template")]
    pub chat_template: String,
    #[serde(default = "default_candle_device")]
    pub device: String,
    #[serde(default)]
    pub embedding_repo: Option<String>,
    #[serde(default)]
    pub generation: GenerationParams,
}

fn default_candle_source() -> String {
    "huggingface".into()
}

fn default_chat_template() -> String {
    "chatml".into()
}

fn default_candle_device() -> String {
    "cpu".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GenerationParams {
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    #[serde(default = "default_seed")]
    pub seed: u64,
    #[serde(default = "default_repeat_penalty")]
    pub repeat_penalty: f32,
    #[serde(default = "default_repeat_last_n")]
    pub repeat_last_n: usize,
}

pub(crate) const MAX_TOKENS_CAP: usize = 32768;

impl GenerationParams {
    #[must_use]
    pub fn capped_max_tokens(&self) -> usize {
        self.max_tokens.min(MAX_TOKENS_CAP)
    }
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            temperature: default_temperature(),
            top_p: None,
            top_k: None,
            max_tokens: default_max_tokens(),
            seed: default_seed(),
            repeat_penalty: default_repeat_penalty(),
            repeat_last_n: default_repeat_last_n(),
        }
    }
}

fn default_temperature() -> f64 {
    0.7
}

fn default_max_tokens() -> usize {
    2048
}

fn default_seed() -> u64 {
    42
}

fn default_repeat_penalty() -> f32 {
    1.1
}

fn default_repeat_last_n() -> usize {
    64
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OrchestratorConfig {
    pub default: String,
    pub embed: String,
    #[serde(default)]
    pub providers: std::collections::HashMap<String, OrchestratorProviderConfig>,
    #[serde(default)]
    pub routes: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct OrchestratorProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SkillsConfig {
    pub paths: Vec<String>,
    #[serde(default = "default_max_active_skills")]
    pub max_active_skills: usize,
    #[serde(default = "default_disambiguation_threshold")]
    pub disambiguation_threshold: f32,
    #[serde(default)]
    pub learning: LearningConfig,
    #[serde(default)]
    pub trust: TrustConfig,
}

fn default_disambiguation_threshold() -> f32 {
    0.05
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TrustConfig {
    #[serde(default = "default_trust_default_level")]
    pub default_level: TrustLevel,
    #[serde(default = "default_trust_local_level")]
    pub local_level: TrustLevel,
    #[serde(default = "default_trust_hash_mismatch_level")]
    pub hash_mismatch_level: TrustLevel,
}

fn default_trust_default_level() -> TrustLevel {
    TrustLevel::Quarantined
}

fn default_trust_local_level() -> TrustLevel {
    TrustLevel::Trusted
}

fn default_trust_hash_mismatch_level() -> TrustLevel {
    TrustLevel::Quarantined
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            default_level: default_trust_default_level(),
            local_level: default_trust_local_level(),
            hash_mismatch_level: default_trust_hash_mismatch_level(),
        }
    }
}

fn default_max_active_skills() -> usize {
    5
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LearningConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auto_activate: bool,
    #[serde(default = "default_min_failures")]
    pub min_failures: u32,
    #[serde(default = "default_improve_threshold")]
    pub improve_threshold: f64,
    #[serde(default = "default_rollback_threshold")]
    pub rollback_threshold: f64,
    #[serde(default = "default_min_evaluations")]
    pub min_evaluations: u32,
    #[serde(default = "default_max_versions")]
    pub max_versions: u32,
    #[serde(default = "default_cooldown_minutes")]
    pub cooldown_minutes: u64,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_activate: false,
            min_failures: default_min_failures(),
            improve_threshold: default_improve_threshold(),
            rollback_threshold: default_rollback_threshold(),
            min_evaluations: default_min_evaluations(),
            max_versions: default_max_versions(),
            cooldown_minutes: default_cooldown_minutes(),
        }
    }
}

fn default_min_failures() -> u32 {
    3
}
fn default_improve_threshold() -> f64 {
    0.7
}
fn default_rollback_threshold() -> f64 {
    0.5
}
fn default_min_evaluations() -> u32 {
    5
}
fn default_max_versions() -> u32 {
    10
}
fn default_cooldown_minutes() -> u64 {
    60
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MemoryConfig {
    pub sqlite_path: String,
    pub history_limit: u32,
    #[serde(default = "default_qdrant_url")]
    pub qdrant_url: String,
    #[serde(default)]
    pub semantic: SemanticConfig,
    #[serde(default = "default_summarization_threshold")]
    pub summarization_threshold: usize,
    #[serde(default = "default_context_budget_tokens")]
    pub context_budget_tokens: usize,
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f32,
    #[serde(default = "default_compaction_preserve_tail")]
    pub compaction_preserve_tail: usize,
    #[serde(default = "default_auto_budget")]
    pub auto_budget: bool,
    #[serde(default = "default_prune_protect_tokens")]
    pub prune_protect_tokens: usize,
    #[serde(default = "default_cross_session_score_threshold")]
    pub cross_session_score_threshold: f32,
}

fn default_qdrant_url() -> String {
    "http://localhost:6334".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct IndexConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_index_watch")]
    pub watch: bool,
    #[serde(default = "default_index_max_chunks")]
    pub max_chunks: usize,
    #[serde(default = "default_index_score_threshold")]
    pub score_threshold: f32,
    #[serde(default = "default_index_budget_ratio")]
    pub budget_ratio: f32,
    #[serde(default = "default_index_repo_map_tokens")]
    pub repo_map_tokens: usize,
    #[serde(default = "default_repo_map_ttl_secs")]
    pub repo_map_ttl_secs: u64,
}

fn default_index_watch() -> bool {
    true
}

fn default_index_max_chunks() -> usize {
    12
}

fn default_index_score_threshold() -> f32 {
    0.25
}

fn default_index_budget_ratio() -> f32 {
    0.40
}

fn default_index_repo_map_tokens() -> usize {
    500
}

fn default_repo_map_ttl_secs() -> u64 {
    300
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            watch: default_index_watch(),
            max_chunks: default_index_max_chunks(),
            score_threshold: default_index_score_threshold(),
            budget_ratio: default_index_budget_ratio(),
            repo_map_tokens: default_index_repo_map_tokens(),
            repo_map_ttl_secs: default_repo_map_ttl_secs(),
        }
    }
}

fn default_summarization_threshold() -> usize {
    50
}

fn default_context_budget_tokens() -> usize {
    0
}

fn default_compaction_threshold() -> f32 {
    0.80
}

fn default_compaction_preserve_tail() -> usize {
    6
}

fn default_auto_budget() -> bool {
    true
}

fn default_prune_protect_tokens() -> usize {
    40_000
}

fn default_cross_session_score_threshold() -> f32 {
    0.35
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SemanticConfig {
    #[serde(default = "default_semantic_enabled")]
    pub enabled: bool,
    #[serde(default = "default_recall_limit")]
    pub recall_limit: usize,
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,
    #[serde(default = "default_keyword_weight")]
    pub keyword_weight: f64,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            enabled: default_semantic_enabled(),
            recall_limit: default_recall_limit(),
            vector_weight: default_vector_weight(),
            keyword_weight: default_keyword_weight(),
        }
    }
}

fn default_semantic_enabled() -> bool {
    true
}

fn default_recall_limit() -> usize {
    5
}

fn default_vector_weight() -> f64 {
    0.7
}

fn default_keyword_weight() -> f64 {
    0.3
}

#[derive(Clone, Deserialize, Serialize)]
pub struct TelegramConfig {
    pub token: Option<String>,
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("allowed_users", &self.allowed_users)
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct DiscordConfig {
    pub token: Option<String>,
    pub application_id: Option<String>,
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub allowed_role_ids: Vec<String>,
    #[serde(default)]
    pub allowed_channel_ids: Vec<String>,
}

impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("application_id", &self.application_id)
            .field("allowed_user_ids", &self.allowed_user_ids)
            .field("allowed_role_ids", &self.allowed_role_ids)
            .field("allowed_channel_ids", &self.allowed_channel_ids)
            .finish()
    }
}

fn default_slack_port() -> u16 {
    3000
}

fn default_slack_webhook_host() -> String {
    "127.0.0.1".into()
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SlackConfig {
    pub bot_token: Option<String>,
    pub signing_secret: Option<String>,
    #[serde(default = "default_slack_webhook_host")]
    pub webhook_host: String,
    #[serde(default = "default_slack_port")]
    pub port: u16,
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub allowed_channel_ids: Vec<String>,
}

impl std::fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackConfig")
            .field("bot_token", &self.bot_token.as_ref().map(|_| "[REDACTED]"))
            .field(
                "signing_secret",
                &self.signing_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("webhook_host", &self.webhook_host)
            .field("port", &self.port)
            .field("allowed_user_ids", &self.allowed_user_ids)
            .field("allowed_channel_ids", &self.allowed_channel_ids)
            .finish()
    }
}

#[derive(Deserialize, Serialize)]
pub struct A2aServerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_a2a_host")]
    pub host: String,
    #[serde(default = "default_a2a_port")]
    pub port: u16,
    #[serde(default)]
    pub public_url: String,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default = "default_a2a_rate_limit")]
    pub rate_limit: u32,
    #[serde(default = "default_true")]
    pub require_tls: bool,
    #[serde(default = "default_true")]
    pub ssrf_protection: bool,
    #[serde(default = "default_a2a_max_body")]
    pub max_body_size: usize,
}

fn default_true() -> bool {
    true
}

fn default_a2a_max_body() -> usize {
    1_048_576
}

impl std::fmt::Debug for A2aServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aServerConfig")
            .field("enabled", &self.enabled)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("public_url", &self.public_url)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("rate_limit", &self.rate_limit)
            .field("require_tls", &self.require_tls)
            .field("ssrf_protection", &self.ssrf_protection)
            .field("max_body_size", &self.max_body_size)
            .finish()
    }
}

fn default_a2a_host() -> String {
    "0.0.0.0".into()
}

fn default_a2a_port() -> u16 {
    8080
}

fn default_a2a_rate_limit() -> u32 {
    60
}

impl Default for A2aServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_a2a_host(),
            port: default_a2a_port(),
            public_url: String::new(),
            auth_token: None,
            rate_limit: default_a2a_rate_limit(),
            require_tls: true,
            ssrf_protection: true,
            max_body_size: default_a2a_max_body(),
        }
    }
}

fn default_llm_timeout() -> u64 {
    120
}

fn default_embedding_timeout() -> u64 {
    30
}

fn default_a2a_timeout() -> u64 {
    30
}

fn default_max_parallel_tools() -> usize {
    8
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct SecurityConfig {
    #[serde(default = "default_true")]
    pub redact_secrets: bool,
    #[serde(default)]
    pub autonomy_level: AutonomyLevel,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            redact_secrets: true,
            autonomy_level: AutonomyLevel::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct TimeoutConfig {
    #[serde(default = "default_llm_timeout")]
    pub llm_seconds: u64,
    #[serde(default = "default_embedding_timeout")]
    pub embedding_seconds: u64,
    #[serde(default = "default_a2a_timeout")]
    pub a2a_seconds: u64,
    #[serde(default = "default_max_parallel_tools")]
    pub max_parallel_tools: usize,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            llm_seconds: default_llm_timeout(),
            embedding_seconds: default_embedding_timeout(),
            a2a_seconds: default_a2a_timeout(),
            max_parallel_tools: default_max_parallel_tools(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default = "default_max_dynamic_servers")]
    pub max_dynamic_servers: usize,
}

fn default_max_dynamic_servers() -> usize {
    10
}

#[derive(Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub id: String,
    /// Stdio transport: command to spawn.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// HTTP transport: remote MCP server URL.
    pub url: Option<String>,
    #[serde(default = "default_mcp_timeout")]
    pub timeout: u64,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted: HashMap<&str, &str> = self
            .env
            .keys()
            .map(|k| (k.as_str(), "[REDACTED]"))
            .collect();
        f.debug_struct("McpServerConfig")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &redacted)
            .field("url", &self.url)
            .field("timeout", &self.timeout)
            .finish()
    }
}

fn default_mcp_timeout() -> u64 {
    30
}

#[derive(Debug, Deserialize, Serialize)]
pub struct VaultConfig {
    #[serde(default = "default_vault_backend")]
    pub backend: String,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            backend: default_vault_backend(),
        }
    }
}

fn default_vault_backend() -> String {
    "env".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CostConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_daily_cents")]
    pub max_daily_cents: u32,
}

fn default_max_daily_cents() -> u32 {
    500
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_daily_cents: default_max_daily_cents(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub exporter: String,
    #[serde(default = "default_otlp_endpoint")]
    pub endpoint: String,
}

fn default_otlp_endpoint() -> String {
    "http://localhost:4317".into()
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            exporter: String::new(),
            endpoint: default_otlp_endpoint(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_gateway_bind")]
    pub bind: String,
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default = "default_gateway_rate_limit")]
    pub rate_limit: u32,
    #[serde(default = "default_gateway_max_body")]
    pub max_body_size: usize,
}

fn default_gateway_bind() -> String {
    "127.0.0.1".into()
}

fn default_gateway_port() -> u16 {
    8090
}

fn default_gateway_rate_limit() -> u32 {
    120
}

fn default_gateway_max_body() -> usize {
    1_048_576
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_gateway_bind(),
            port: default_gateway_port(),
            auth_token: None,
            rate_limit: default_gateway_rate_limit(),
            max_body_size: default_gateway_max_body(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_pid_file")]
    pub pid_file: String,
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
    #[serde(default = "default_max_restart_backoff")]
    pub max_restart_backoff_secs: u64,
}

fn default_pid_file() -> String {
    "~/.zeph/zeph.pid".into()
}

fn default_health_interval() -> u64 {
    30
}

fn default_max_restart_backoff() -> u64 {
    60
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            pid_file: default_pid_file(),
            health_interval_secs: default_health_interval(),
            max_restart_backoff_secs: default_max_restart_backoff(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SchedulerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub tasks: Vec<ScheduledTaskConfig>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct TuiConfig {
    #[serde(default)]
    pub show_source_labels: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduledTaskConfig {
    pub name: String,
    pub cron: String,
    pub kind: String,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Default)]
pub struct ResolvedSecrets {
    pub claude_api_key: Option<Secret>,
    pub openai_api_key: Option<Secret>,
    pub compatible_api_keys: HashMap<String, Secret>,
    pub discord_token: Option<Secret>,
    pub slack_bot_token: Option<Secret>,
    pub slack_signing_secret: Option<Secret>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agent: AgentConfig {
                name: "Zeph".into(),
                max_tool_iterations: 10,
                summary_model: None,
                auto_update_check: default_auto_update_check(),
            },
            llm: LlmConfig {
                provider: ProviderKind::Ollama,
                base_url: "http://localhost:11434".into(),
                model: "mistral:7b".into(),
                embedding_model: default_embedding_model(),
                cloud: None,
                openai: None,
                candle: None,
                orchestrator: None,
                compatible: None,
                router: None,
                stt: None,
                vision_model: None,
            },
            skills: SkillsConfig {
                paths: vec!["./skills".into()],
                max_active_skills: default_max_active_skills(),
                disambiguation_threshold: default_disambiguation_threshold(),
                learning: LearningConfig::default(),
                trust: TrustConfig::default(),
            },
            memory: MemoryConfig {
                sqlite_path: "./data/zeph.db".into(),
                history_limit: 50,
                qdrant_url: default_qdrant_url(),
                semantic: SemanticConfig::default(),
                summarization_threshold: default_summarization_threshold(),
                context_budget_tokens: default_context_budget_tokens(),
                compaction_threshold: default_compaction_threshold(),
                compaction_preserve_tail: default_compaction_preserve_tail(),
                auto_budget: default_auto_budget(),
                prune_protect_tokens: default_prune_protect_tokens(),
                cross_session_score_threshold: default_cross_session_score_threshold(),
            },
            telegram: None,
            discord: None,
            slack: None,
            tools: ToolsConfig::default(),
            a2a: A2aServerConfig::default(),
            mcp: McpConfig::default(),
            index: IndexConfig::default(),
            vault: VaultConfig::default(),
            security: SecurityConfig::default(),
            timeouts: TimeoutConfig::default(),
            cost: CostConfig::default(),
            observability: ObservabilityConfig::default(),
            gateway: GatewayConfig::default(),
            daemon: DaemonConfig::default(),
            scheduler: SchedulerConfig::default(),
            tui: TuiConfig::default(),
            secrets: ResolvedSecrets::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_serialize_roundtrip() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).expect("serialize");
        let back: Config = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(back.agent.name, config.agent.name);
        assert_eq!(back.llm.provider, config.llm.provider);
        assert_eq!(back.llm.model, config.llm.model);
        assert_eq!(back.memory.sqlite_path, config.memory.sqlite_path);
        assert_eq!(back.memory.history_limit, config.memory.history_limit);
        assert_eq!(back.vault.backend, config.vault.backend);
        assert_eq!(back.agent.auto_update_check, config.agent.auto_update_check);
    }

    #[test]
    fn config_default_snapshot() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).expect("serialize");
        insta::assert_snapshot!(toml_str);
    }

    #[test]
    fn generation_params_defaults() {
        let p = GenerationParams::default();
        assert!((p.temperature - 0.7).abs() < f64::EPSILON);
        assert_eq!(p.max_tokens, 2048);
        assert_eq!(p.seed, 42);
    }
}
