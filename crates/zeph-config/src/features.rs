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

fn default_scheduler_daemon_tick_secs() -> u64 {
    60
}

fn default_scheduler_daemon_shutdown_grace_secs() -> u64 {
    30
}

fn default_scheduler_daemon_pid_file() -> String {
    // MINOR-4: dirs::state_dir() is None on macOS, so we use platform-specific fallbacks.
    #[cfg(target_os = "macos")]
    {
        dirs::data_local_dir()
            .map_or_else(
                || std::path::PathBuf::from("~/.zeph/zeph.pid"),
                |d| d.join("zeph").join("zeph.pid"),
            )
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .map_or_else(
                || std::path::PathBuf::from("~/.zeph/zeph.pid"),
                |d| d.join("zeph").join("zeph.pid"),
            )
            .to_string_lossy()
            .into_owned()
    }
}

fn default_scheduler_daemon_log_file() -> String {
    #[cfg(target_os = "macos")]
    {
        // macOS: ~/Library/Logs/zeph/zeph.log
        dirs::cache_dir()
            .map_or_else(
                || std::path::PathBuf::from("~/.zeph/zeph.log"),
                |d| d.join("zeph").join("zeph.log"),
            )
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .map_or_else(
                || std::path::PathBuf::from("~/.zeph/zeph.log"),
                |d| d.join("zeph").join("zeph.log"),
            )
            .to_string_lossy()
            .into_owned()
    }
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

/// Skill discovery and matching configuration, nested under `[skills]` in TOML.
///
/// Controls where skills are loaded from, how they are ranked during retrieval,
/// the RL re-ranking head, NL skill generation, and automated skill mining.
///
/// # Example (TOML)
///
/// ```toml
/// [skills]
/// paths = ["~/.config/zeph/skills"]
/// max_active_skills = 5
/// disambiguation_threshold = 0.20
/// hybrid_search = true
/// ```
#[derive(Debug, Deserialize, Serialize)]
pub struct SkillsConfig {
    /// Directories to scan for `*.skill.md` / `SKILL.md` files.
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
    /// External-feedback skill evaluator configuration (#3319).
    #[serde(default)]
    pub evaluation: SkillEvaluationConfig,
    /// Proactive world-knowledge exploration configuration (#3320).
    #[serde(default)]
    pub proactive_exploration: ProactiveExplorationConfig,
}

// --- SkillEvaluationConfig defaults ---

fn default_skill_quality_threshold() -> f32 {
    0.60
}

fn default_weight_correctness() -> f32 {
    0.50
}

fn default_weight_reusability() -> f32 {
    0.25
}

fn default_weight_specificity() -> f32 {
    0.25
}

fn default_eval_fail_open() -> bool {
    true
}

fn default_skill_eval_timeout_ms() -> u64 {
    15_000
}

/// External-feedback skill evaluator configuration, nested under `[skills.evaluation]` in TOML.
///
/// When `enabled = true`, generated SKILL.md files are scored by a critic LLM before being
/// written to disk. Skills below `quality_threshold` are rejected.
///
/// # Weights
///
/// `weight_correctness + weight_reusability + weight_specificity` must equal `1.0 ± 1e-3`.
/// Starting defaults (0.50 / 0.25 / 0.25) are intuition-based and will be tuned after
/// real-world telemetry is collected.
///
/// # Example (TOML)
///
/// ```toml
/// [skills.evaluation]
/// enabled = true
/// provider = "fast"
/// quality_threshold = 0.60
/// fail_open_on_error = true
/// timeout_ms = 15000
/// ```
#[derive(Debug, Deserialize, Serialize)]
pub struct SkillEvaluationConfig {
    /// Enable the evaluator gate. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Provider name for the critic LLM. Empty = primary provider.
    #[serde(default)]
    pub provider: ProviderName,
    /// Minimum composite score required to accept a generated skill. Default: `0.60`.
    #[serde(default = "default_skill_quality_threshold")]
    pub quality_threshold: f32,
    /// Weight for `correctness` in the composite score. Default: `0.50`.
    #[serde(default = "default_weight_correctness")]
    pub weight_correctness: f32,
    /// Weight for `reusability` in the composite score. Default: `0.25`.
    #[serde(default = "default_weight_reusability")]
    pub weight_reusability: f32,
    /// Weight for `specificity` in the composite score. Default: `0.25`.
    #[serde(default = "default_weight_specificity")]
    pub weight_specificity: f32,
    /// Fail-open policy: accept skill when the evaluator call fails. Default: `true`.
    #[serde(default = "default_eval_fail_open")]
    pub fail_open_on_error: bool,
    /// Maximum wait for the critic LLM in milliseconds. Default: `15000`.
    #[serde(default = "default_skill_eval_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for SkillEvaluationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: ProviderName::default(),
            quality_threshold: default_skill_quality_threshold(),
            weight_correctness: default_weight_correctness(),
            weight_reusability: default_weight_reusability(),
            weight_specificity: default_weight_specificity(),
            fail_open_on_error: default_eval_fail_open(),
            timeout_ms: default_skill_eval_timeout_ms(),
        }
    }
}

// --- ProactiveExplorationConfig defaults ---

fn default_proactive_max_chars() -> usize {
    8_000
}

fn default_proactive_timeout_ms() -> u64 {
    30_000
}

/// Proactive world-knowledge exploration configuration, nested under `[skills.proactive_exploration]` in TOML.
///
/// When `enabled = true`, the agent inspects each incoming query for a recognisable domain
/// keyword (rust, python, docker, etc.) and generates a SKILL.md for that domain if one
/// does not already exist. The skill is written to `output_dir` and registered in the
/// skill registry; it becomes visible to the matcher on the **next** turn (next-turn
/// visibility is intentional — see codebase comment in `ProactiveExplorer`).
///
/// # Example (TOML)
///
/// ```toml
/// [skills.proactive_exploration]
/// enabled = true
/// output_dir = "~/.config/zeph/skills/generated"
/// provider = "fast"
/// ```
#[derive(Debug, Deserialize, Serialize)]
pub struct ProactiveExplorationConfig {
    /// Enable proactive exploration. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Provider name for skill generation. Empty = primary provider.
    #[serde(default)]
    pub provider: ProviderName,
    /// Directory where generated skills are written. Defaults to first `skills.paths` entry.
    #[serde(default)]
    pub output_dir: Option<String>,
    /// Maximum SKILL.md body size in characters. Default: `8000`.
    #[serde(default = "default_proactive_max_chars")]
    pub max_chars: usize,
    /// Per-exploration timeout in milliseconds. Default: `30000`.
    #[serde(default = "default_proactive_timeout_ms")]
    pub timeout_ms: u64,
    /// Domain names to skip exploration for (e.g. `["rust"]` to suppress auto-generation
    /// if you maintain your own Rust skill). Default: `[]`.
    #[serde(default)]
    pub excluded_domains: Vec<String>,
}

impl Default for ProactiveExplorationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: ProviderName::default(),
            output_dir: None,
            max_chars: default_proactive_max_chars(),
            timeout_ms: default_proactive_timeout_ms(),
            excluded_domains: Vec::new(),
        }
    }
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

/// Code indexing and repo-map configuration, nested under `[index]` in TOML.
///
/// When `enabled = true`, the agent indexes source files into Qdrant for semantic
/// code search. The repo map is injected into the system prompt or served via
/// `IndexMcpServer` tool calls when `mcp_enabled = true`.
///
/// # Example (TOML)
///
/// ```toml
/// [index]
/// enabled = true
/// watch = false
/// max_chunks = 12
/// score_threshold = 0.25
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct IndexConfig {
    /// Enable code indexing. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Enable semantic code search tool. Default: `true` (no-op when `enabled = false`).
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

/// Vault backend configuration, nested under `[vault]` in TOML.
///
/// Selects how API keys and secrets are resolved at startup.
///
/// # Example (TOML)
///
/// ```toml
/// [vault]
/// backend = "age"
/// ```
#[derive(Debug, Deserialize, Serialize)]
pub struct VaultConfig {
    /// Vault backend identifier (`"age"`, `"env"`, or `"keyring"`). Default: `"env"`.
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

/// Cost tracking and budget configuration, nested under `[cost]` in TOML.
///
/// When `enabled = true`, token costs are accumulated per session and displayed in
/// the TUI. When `max_daily_cents > 0`, the agent refuses new turns once the daily
/// budget is exhausted.
///
/// # Example (TOML)
///
/// ```toml
/// [cost]
/// enabled = true
/// max_daily_cents = 500  # $5.00 per day
/// ```
#[derive(Debug, Deserialize, Serialize)]
pub struct CostConfig {
    /// Track and display token costs. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Daily spending cap in US cents (`0` = unlimited). Default: `0`.
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

/// HTTP webhook gateway configuration, nested under `[gateway]` in TOML.
///
/// When `enabled = true`, an HTTP server accepts webhook payloads and injects them
/// as user messages into the agent. Requires the `gateway` feature flag.
///
/// # Example (TOML)
///
/// ```toml
/// [gateway]
/// enabled = true
/// bind = "127.0.0.1"
/// port = 8090
/// auth_token = "secret"
/// rate_limit = 60
/// max_body_size = 1048576
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayConfig {
    /// Enable the HTTP gateway. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// IP address to bind the gateway to. Default: `"127.0.0.1"`.
    #[serde(default = "default_gateway_bind")]
    pub bind: String,
    /// Port to listen on. Default: `8090`.
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    /// Bearer token for request authentication. When set, all requests must include
    /// `Authorization: Bearer <token>`. Default: `None` (no auth).
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Maximum requests per minute. Must be `> 0`. Default: `120`.
    #[serde(default = "default_gateway_rate_limit")]
    pub rate_limit: u32,
    /// Maximum request body size in bytes. Must be `<= 10 MiB`. Default: `1048576` (1 MiB).
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

/// Daemon / process supervisor configuration, nested under `[daemon]` in TOML.
///
/// When `enabled = true`, Zeph runs as a background process with automatic restart
/// and health monitoring.
///
/// # Example (TOML)
///
/// ```toml
/// [daemon]
/// enabled = true
/// pid_file = "~/.zeph/zeph.pid"
/// health_interval_secs = 30
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonConfig {
    /// Run Zeph as a background daemon. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the PID file written at daemon startup. Default: `"~/.zeph/zeph.pid"`.
    #[serde(default = "default_pid_file")]
    pub pid_file: String,
    /// Interval in seconds between health checks. Default: `30`.
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
    /// Maximum backoff in seconds between restart attempts. Default: `60`.
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

/// Daemon mode configuration for `zeph serve`, nested under `[scheduler.daemon]` in TOML.
///
/// Controls the behaviour of the background scheduler process started by `zeph serve`.
/// The pid file **must be on a local filesystem**; NFS mounts may not provide reliable
/// exclusive locking.
///
/// Log rotation requires `logrotate copytruncate` or a SIGHUP signal; the daemon does
/// not rotate logs internally (append-only log file).
///
/// # Platform defaults
///
/// - **macOS**: pid `~/Library/Application Support/zeph/zeph.pid`,
///   log `~/Library/Caches/zeph/zeph.log`
/// - **Linux**: pid `$XDG_STATE_HOME/zeph/zeph.pid`,
///   log `$XDG_STATE_HOME/zeph/zeph.log`
///
/// # Example (TOML)
///
/// ```toml
/// [scheduler.daemon]
/// pid_file  = "~/.local/state/zeph/zeph.pid"
/// log_file  = "~/.local/state/zeph/zeph.log"
/// catch_up  = true
/// tick_secs = 60
/// shutdown_grace_secs = 30
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerDaemonConfig {
    /// Path to the PID file. Must reside on a local filesystem for reliable locking.
    #[serde(default = "default_scheduler_daemon_pid_file")]
    pub pid_file: String,
    /// Path to the daemon log file (append-only; rotated externally).
    #[serde(default = "default_scheduler_daemon_log_file")]
    pub log_file: String,
    /// When `true`, fire overdue periodic tasks once on startup before entering the
    /// regular tick loop. At most one missed occurrence per task is replayed.
    #[serde(default = "crate::defaults::default_true")]
    pub catch_up: bool,
    /// Tick interval in seconds (clamped to `5..=3600`). Default: `60`.
    #[serde(default = "default_scheduler_daemon_tick_secs")]
    pub tick_secs: u64,
    /// Graceful shutdown window in seconds: how long to wait for in-flight tasks
    /// after a SIGTERM before forcing an exit. Default: `30`.
    #[serde(default = "default_scheduler_daemon_shutdown_grace_secs")]
    pub shutdown_grace_secs: u64,
}

impl Default for SchedulerDaemonConfig {
    fn default() -> Self {
        Self {
            pid_file: default_scheduler_daemon_pid_file(),
            log_file: default_scheduler_daemon_log_file(),
            catch_up: true,
            tick_secs: default_scheduler_daemon_tick_secs(),
            shutdown_grace_secs: default_scheduler_daemon_shutdown_grace_secs(),
        }
    }
}

/// Cron-based task scheduler configuration, nested under `[scheduler]` in TOML.
///
/// When `enabled = true`, the scheduler runs periodic tasks on a cron schedule.
/// Requires the `scheduler` feature flag.
///
/// # Example (TOML)
///
/// ```toml
/// [scheduler]
/// enabled = true
/// tick_interval_secs = 60
/// max_tasks = 20
///
/// [[scheduler.tasks]]
/// name = "daily-summary"
/// cron = "0 9 * * *"
/// kind = "prompt"
/// prompt = "Summarize what was accomplished today."
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerConfig {
    /// Enable the task scheduler. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// How often the scheduler checks for due tasks, in seconds. Default: `60`.
    #[serde(default = "default_scheduler_tick_interval")]
    pub tick_interval_secs: u64,
    /// Maximum number of scheduled tasks allowed. Default: `100`.
    #[serde(default = "default_scheduler_max_tasks")]
    pub max_tasks: usize,
    /// List of scheduled task definitions.
    #[serde(default)]
    pub tasks: Vec<ScheduledTaskConfig>,
    /// Daemon lifecycle settings used by `zeph serve` / `zeph stop` / `zeph status`.
    #[serde(default)]
    pub daemon: SchedulerDaemonConfig,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tick_interval_secs: default_scheduler_tick_interval(),
            max_tasks: default_scheduler_max_tasks(),
            tasks: Vec::new(),
            daemon: SchedulerDaemonConfig::default(),
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

/// A single scheduled task entry, nested under `[[scheduler.tasks]]` in TOML.
///
/// Either `cron` (recurring) or `run_at` (one-shot ISO 8601 datetime) must be set.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduledTaskConfig {
    /// Unique task name used in logs and the scheduler database.
    pub name: String,
    /// Cron expression for recurring tasks (e.g. `"0 9 * * *"` for daily at 09:00).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// One-shot ISO 8601 datetime for one-time tasks. Ignored when `cron` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_at: Option<String>,
    /// Determines which built-in handler executes this task.
    pub kind: ScheduledTaskKind,
    /// Arbitrary JSON configuration forwarded to the task handler.
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

// --- CompressionSpectrumConfig defaults ---

fn default_compression_spectrum_promotion_window() -> usize {
    200
}

fn default_compression_spectrum_min_occurrences() -> u32 {
    3
}

fn default_compression_spectrum_min_sessions() -> u32 {
    2
}

fn default_compression_spectrum_cluster_threshold() -> f32 {
    0.85
}

fn default_retrieval_low_budget_ratio() -> f32 {
    0.20
}

fn default_retrieval_mid_budget_ratio() -> f32 {
    0.50
}

/// Experience compression spectrum configuration, nested under `[memory.compression_spectrum]`.
///
/// When `enabled = true`, the agent uses a three-tier memory retrieval policy
/// (Episodic → Procedural → Declarative) keyed on remaining token budget, and
/// runs a background promotion engine that converts recurring episodic patterns
/// into generated SKILL.md files.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.compression_spectrum]
/// enabled = true
/// promotion_output_dir = "~/.config/zeph/skills/promoted"
/// promotion_provider = "quality"
/// ```
#[derive(Debug, Deserialize, Serialize)]
pub struct CompressionSpectrumConfig {
    /// Enable the compression spectrum. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Directory where promoted SKILL.md files are written.
    #[serde(default)]
    pub promotion_output_dir: Option<String>,
    /// Provider name for SKILL.md generation during promotion. Empty = primary provider.
    #[serde(default)]
    pub promotion_provider: ProviderName,
    /// Maximum number of recent episodic messages to scan for promotion candidates.
    /// Default: `200`.
    #[serde(default = "default_compression_spectrum_promotion_window")]
    pub promotion_window: usize,
    /// Minimum number of times a pattern must appear across all sessions to be promoted.
    /// Default: `3`.
    #[serde(default = "default_compression_spectrum_min_occurrences")]
    pub min_occurrences: u32,
    /// Minimum number of distinct sessions containing the pattern. Default: `2`.
    #[serde(default = "default_compression_spectrum_min_sessions")]
    pub min_sessions: u32,
    /// Cosine similarity threshold for clustering episodic messages. Default: `0.85`.
    #[serde(default = "default_compression_spectrum_cluster_threshold")]
    pub cluster_threshold: f32,
    /// Remaining-token ratio below which only episodic recall is used. Default: `0.20`.
    #[serde(default = "default_retrieval_low_budget_ratio")]
    pub retrieval_low_budget_ratio: f32,
    /// Remaining-token ratio below which episodic + procedural recall is used. Default: `0.50`.
    #[serde(default = "default_retrieval_mid_budget_ratio")]
    pub retrieval_mid_budget_ratio: f32,
}

impl Default for CompressionSpectrumConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            promotion_output_dir: None,
            promotion_provider: ProviderName::default(),
            promotion_window: default_compression_spectrum_promotion_window(),
            min_occurrences: default_compression_spectrum_min_occurrences(),
            min_sessions: default_compression_spectrum_min_sessions(),
            cluster_threshold: default_compression_spectrum_cluster_threshold(),
            retrieval_low_budget_ratio: default_retrieval_low_budget_ratio(),
            retrieval_mid_budget_ratio: default_retrieval_mid_budget_ratio(),
        }
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
    /// Default: `"http://localhost:4317"`.
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

/// Debug dump configuration, nested under `[debug]` in TOML.
///
/// When `enabled = true`, LLM request/response payloads are written to disk for inspection.
/// Each session creates a subdirectory under `output_dir` named by session ID.
///
/// # Example (TOML)
///
/// ```toml
/// [debug]
/// enabled = true
/// format = "raw"
/// ```
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
