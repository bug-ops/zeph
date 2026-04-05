// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use crate::defaults::{default_skill_paths, default_true};
use crate::learning::LearningConfig;
use crate::providers::ProviderName;
use crate::security::TrustConfig;

fn default_disambiguation_threshold() -> f32 {
    0.20
}

fn default_rl_learning_rate() -> f32 {
    0.01
}

fn default_rl_weight() -> f32 {
    0.3
}

fn default_rl_persist_interval() -> u32 {
    10
}

fn default_rl_warmup_updates() -> u32 {
    50
}

fn default_min_injection_score() -> f32 {
    0.20
}

fn default_cosine_weight() -> f32 {
    0.7
}

fn default_hybrid_search() -> bool {
    true
}

fn default_max_active_skills() -> usize {
    5
}

fn default_index_watch() -> bool {
    // Default off: watcher watches ALL files recursively and bypasses gitignore
    // filtering at the OS level. Projects with large .local/ or target/ directories
    // trigger continuous reindex loops, causing unbounded memory growth.
    // Users must explicitly opt in with `[index] watch = true`.
    false
}

fn default_index_search_enabled() -> bool {
    true
}

fn default_index_max_chunks() -> usize {
    12
}

fn default_index_concurrency() -> usize {
    4
}

fn default_index_batch_size() -> usize {
    32
}

fn default_index_memory_batch_size() -> usize {
    32
}

fn default_index_max_file_bytes() -> usize {
    512 * 1024
}

fn default_index_embed_concurrency() -> usize {
    2
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

fn default_vault_backend() -> String {
    "env".into()
}

fn default_max_daily_cents() -> u32 {
    0
}

fn default_otlp_endpoint() -> String {
    "http://localhost:4317".into()
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

fn default_scheduler_tick_interval() -> u64 {
    60
}

fn default_scheduler_max_tasks() -> usize {
    100
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

/// Controls how skills are formatted in the system prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillPromptMode {
    Full,
    Compact,
    #[default]
    Auto,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SkillsConfig {
    #[serde(default = "default_skill_paths")]
    pub paths: Vec<String>,
    #[serde(default = "default_max_active_skills")]
    pub max_active_skills: usize,
    #[serde(default = "default_disambiguation_threshold")]
    pub disambiguation_threshold: f32,
    #[serde(default = "default_min_injection_score")]
    pub min_injection_score: f32,
    #[serde(default = "default_cosine_weight")]
    pub cosine_weight: f32,
    #[serde(default = "default_hybrid_search")]
    pub hybrid_search: bool,
    #[serde(default)]
    pub learning: LearningConfig,
    #[serde(default)]
    pub trust: TrustConfig,
    #[serde(default)]
    pub prompt_mode: SkillPromptMode,
    /// Enable two-stage category-first skill matching (requires `category` set in SKILL.md).
    /// Falls back to flat matching when no multi-skill categories are available.
    #[serde(default)]
    pub two_stage_matching: bool,
    /// Warn when any two skills have cosine similarity ≥ this threshold.
    /// Set to 0.0 (default) to disable the confusability check entirely.
    #[serde(default)]
    pub confusability_threshold: f32,

    // --- SkillOrchestra: RL routing head ---
    /// Enable RL routing head for skill re-ranking (disabled by default).
    #[serde(default)]
    pub rl_routing_enabled: bool,
    /// Learning rate for REINFORCE weight updates.
    #[serde(default = "default_rl_learning_rate")]
    pub rl_learning_rate: f32,
    /// Blend weight: `final_score = (1-rl_weight)*cosine + rl_weight*rl_score`.
    #[serde(default = "default_rl_weight")]
    pub rl_weight: f32,
    /// Persist weights every N updates (0 = persist every update).
    #[serde(default = "default_rl_persist_interval")]
    pub rl_persist_interval: u32,
    /// Skip RL blending for the first N updates (cold-start warmup).
    #[serde(default = "default_rl_warmup_updates")]
    pub rl_warmup_updates: u32,
    /// Embedding dimension for the RL routing head.
    /// Must match the output dimension of the configured embedding provider.
    /// Defaults to `None` → 1536 (`text-embedding-3-small` output dimension).
    #[serde(default)]
    pub rl_embed_dim: Option<usize>,

    // --- NL skill generation ---
    /// Provider name for `/skill create` NL generation. Empty = primary provider.
    #[serde(default)]
    pub generation_provider: ProviderName,
    /// Directory where generated skills are written. Defaults to first entry in `paths`.
    #[serde(default)]
    pub generation_output_dir: Option<String>,
    /// Skill mining configuration.
    #[serde(default)]
    pub mining: SkillMiningConfig,
}

fn default_max_repos_per_query() -> usize {
    20
}

fn default_dedup_threshold() -> f32 {
    0.85
}

fn default_rate_limit_rpm() -> u32 {
    25
}

/// Configuration for the automated skill mining pipeline (`zeph-skills-miner` binary).
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct SkillMiningConfig {
    /// GitHub search queries for repo discovery (e.g. "topic:cli-tool language:rust stars:>100").
    #[serde(default)]
    pub queries: Vec<String>,
    /// Maximum repos to fetch per query (capped at 100 by GitHub API). Default: 20.
    #[serde(default = "default_max_repos_per_query")]
    pub max_repos_per_query: usize,
    /// Cosine similarity threshold for dedup against existing skills. Default: 0.85.
    #[serde(default = "default_dedup_threshold")]
    pub dedup_threshold: f32,
    /// Output directory for mined skills.
    #[serde(default)]
    pub output_dir: Option<String>,
    /// Provider name for skill generation during mining. Empty = primary provider.
    #[serde(default)]
    pub generation_provider: ProviderName,
    /// Provider name for embedding during dedup. Empty = primary provider.
    #[serde(default)]
    pub embedding_provider: ProviderName,
    /// Maximum GitHub search requests per minute. Default: 25.
    #[serde(default = "default_rate_limit_rpm")]
    pub rate_limit_rpm: u32,
}

#[derive(Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct IndexConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_index_search_enabled")]
    pub search_enabled: bool,
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
    /// Enable `IndexMcpServer` tools (`symbol_definition`, `find_text_references`, `call_graph`,
    /// `module_summary`). When `true`, static repo-map injection is skipped and the LLM
    /// uses on-demand tool calls instead.
    #[serde(default)]
    pub mcp_enabled: bool,
    /// Root directory to index. When `None`, falls back to the current working directory at
    /// startup. Relative paths are resolved relative to the process working directory.
    #[serde(default)]
    pub workspace_root: Option<std::path::PathBuf>,
    /// Number of files to process concurrently during initial indexing. Default: 4.
    #[serde(default = "default_index_concurrency")]
    pub concurrency: usize,
    /// Maximum number of new chunks to batch into a single Qdrant upsert per file. Default: 32.
    #[serde(default = "default_index_batch_size")]
    pub batch_size: usize,
    /// Number of files to process per memory batch during initial indexing.
    /// After each batch the stream is dropped and the executor yields to allow
    /// the allocator to reclaim pages. Default: `32`.
    #[serde(default = "default_index_memory_batch_size")]
    pub memory_batch_size: usize,
    /// Maximum file size in bytes to index. Files larger than this are skipped.
    /// Protects against large generated files (e.g. lock files, minified JS).
    /// Default: 512 KiB.
    #[serde(default = "default_index_max_file_bytes")]
    pub max_file_bytes: usize,
    /// Name of a `[[llm.providers]]` entry to use exclusively for embedding calls during
    /// indexing. A dedicated provider prevents the indexer from contending with the guardrail
    /// at the API server level (rate limits, Ollama single-model lock). When unset or empty,
    /// falls back to the main agent provider.
    #[serde(default)]
    pub embed_provider: Option<String>,
    /// Maximum parallel `embed_batch` calls during indexing (default: 2 to stay within provider
    /// TPM limits).
    #[serde(default = "default_index_embed_concurrency")]
    pub embed_concurrency: usize,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            search_enabled: default_index_search_enabled(),
            watch: default_index_watch(),
            max_chunks: default_index_max_chunks(),
            score_threshold: default_index_score_threshold(),
            budget_ratio: default_index_budget_ratio(),
            repo_map_tokens: default_index_repo_map_tokens(),
            repo_map_ttl_secs: default_repo_map_ttl_secs(),
            mcp_enabled: false,
            workspace_root: None,
            concurrency: default_index_concurrency(),
            batch_size: default_index_batch_size(),
            memory_batch_size: default_index_memory_batch_size(),
            max_file_bytes: default_index_max_file_bytes(),
            embed_provider: None,
            embed_concurrency: default_index_embed_concurrency(),
        }
    }
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

#[derive(Debug, Deserialize, Serialize)]
pub struct CostConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_daily_cents")]
    pub max_daily_cents: u32,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            enabled: true,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_scheduler_tick_interval")]
    pub tick_interval_secs: u64,
    #[serde(default = "default_scheduler_max_tasks")]
    pub max_tasks: usize,
    #[serde(default)]
    pub tasks: Vec<ScheduledTaskConfig>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tick_interval_secs: default_scheduler_tick_interval(),
            max_tasks: default_scheduler_max_tasks(),
            tasks: Vec::new(),
        }
    }
}

/// Task kind for scheduled tasks.
///
/// Known variants map to built-in handlers; `Custom` accommodates user-defined task types.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskKind {
    MemoryCleanup,
    SkillRefresh,
    HealthCheck,
    UpdateCheck,
    Experiment,
    Custom(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduledTaskConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_at: Option<String>,
    pub kind: ScheduledTaskKind,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_config_defaults() {
        let cfg = IndexConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.search_enabled);
        assert!(!cfg.watch);
        assert_eq!(cfg.concurrency, 4);
        assert_eq!(cfg.batch_size, 32);
        assert!(cfg.workspace_root.is_none());
    }

    #[test]
    fn index_config_serde_roundtrip_with_new_fields() {
        let toml = r#"
            enabled = true
            concurrency = 8
            batch_size = 16
            workspace_root = "/tmp/myproject"
        "#;
        let cfg: IndexConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.concurrency, 8);
        assert_eq!(cfg.batch_size, 16);
        assert_eq!(
            cfg.workspace_root,
            Some(std::path::PathBuf::from("/tmp/myproject"))
        );
        // Re-serialize and deserialize
        let serialized = toml::to_string(&cfg).unwrap();
        let cfg2: IndexConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg2.concurrency, 8);
        assert_eq!(cfg2.batch_size, 16);
    }

    #[test]
    fn index_config_backward_compat_old_toml_without_new_fields() {
        // Old config without workspace_root, concurrency, batch_size — must still parse
        // and use defaults for the missing fields.
        let toml = "
            enabled = true
            max_chunks = 20
            score_threshold = 0.3
        ";
        let cfg: IndexConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_chunks, 20);
        assert!(cfg.workspace_root.is_none());
        assert_eq!(cfg.concurrency, 4);
        assert_eq!(cfg.batch_size, 32);
    }

    #[test]
    fn index_config_workspace_root_none_by_default() {
        let cfg: IndexConfig = toml::from_str("enabled = false").unwrap();
        assert!(cfg.workspace_root.is_none());
    }
}

fn default_trace_service_name() -> String {
    "zeph".into()
}

/// Configuration for OTel-compatible trace dumps (`format = "trace"`).
///
/// When `format = "trace"`, the `TracingCollector` writes a `trace.json` file in OTLP JSON
/// format at session end. Legacy numbered dump files are NOT written by default (C-03).
/// When the `otel` feature is enabled and `otlp_endpoint` is set, spans are also exported
/// via OTLP gRPC.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TraceConfig {
    /// OTLP gRPC endpoint (only used when `otel` feature is enabled).
    /// Defaults to `observability.endpoint` if unset (I-01).
    #[serde(default = "default_otlp_endpoint")]
    pub otlp_endpoint: String,
    /// Service name reported to the `OTel` collector.
    #[serde(default = "default_trace_service_name")]
    pub service_name: String,
    /// Redact sensitive data in span attributes (default: `true`) (C-01).
    #[serde(default = "default_true")]
    pub redact: bool,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: default_otlp_endpoint(),
            service_name: default_trace_service_name(),
            redact: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DebugConfig {
    /// Enable debug dump on startup (CLI `--debug-dump` takes priority).
    pub enabled: bool,
    /// Directory where per-session debug dump subdirectories are created.
    #[serde(default = "crate::defaults::default_debug_output_dir")]
    pub output_dir: std::path::PathBuf,
    /// Output format: `"json"` (default), `"raw"` (API payload), or `"trace"` (OTLP spans).
    pub format: crate::dump_format::DumpFormat,
    /// `OTel` trace configuration (only used when `format = "trace"`).
    pub traces: TraceConfig,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            output_dir: super::defaults::default_debug_output_dir(),
            format: crate::dump_format::DumpFormat::default(),
            traces: TraceConfig::default(),
        }
    }
}
