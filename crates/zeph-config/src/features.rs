// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::num::NonZeroUsize;

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

fn default_bm25_alpha() -> f32 {
    0.7
}

fn default_max_active_skills() -> NonZeroUsize {
    NonZeroUsize::new(5).expect("5 is non-zero")
}

/// Default value for [`SkillsConfig::subagent_skill_token_budget`].
///
/// Exposed as `pub` (unlike this file's other `default_*` helpers) so `zeph-core` can source
/// its `SkillState` construction-time default from the same constant instead of duplicating
/// the literal, which would otherwise be free to drift from the config default over time.
///
/// # Panics
///
/// Never panics in practice — `12_000` is a non-zero literal.
#[must_use]
pub fn default_subagent_skill_token_budget() -> NonZeroUsize {
    NonZeroUsize::new(12_000).expect("12000 is non-zero")
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
    2
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

fn default_initial_pass_batch_delay_ms() -> u64 {
    75
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

fn default_vault_backend() -> VaultBackend {
    VaultBackend::Age
}

/// Selects the vault backend used to resolve secrets at startup.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VaultBackend {
    /// Resolve secrets from environment variables. Zero-config, but weaker than `age` —
    /// not recommended for production use (see spec-010).
    Env,
    /// Resolve secrets from an age-encrypted vault file (default, recommended).
    #[default]
    Age,
    /// Resolve secrets from the OS keyring.
    Keyring,
}

impl std::fmt::Display for VaultBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env => f.write_str("env"),
            Self::Age => f.write_str("age"),
            Self::Keyring => f.write_str("keyring"),
        }
    }
}

fn default_max_daily_cents() -> u32 {
    2500
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

fn default_scheduler_handler_timeout_secs() -> u64 {
    300
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

fn default_gateway_webhook_send_timeout_secs() -> u64 {
    5
}

/// Controls how skills are formatted in the system prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SkillPromptMode {
    Full,
    Compact,
    #[default]
    Auto,
}

/// Identifies which `zeph_plugins::marketplace::RegistryClient` implementation to use for
/// `[skills.registry]` (FR-005, NFR-003). Not an intra-doc link: `zeph-plugins` is not a
/// dependency of this crate (layering) and the type is additionally feature-gated there.
///
/// Deliberately a plain enum defined here in `zeph-config` (Layer 1), never re-exported from
/// the feature-gated `zeph-plugins::marketplace` module — config parsing must always compile
/// regardless of whether the `registry` Cargo feature is enabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum RegistryBackendKind {
    /// The public [skills.sh](https://www.skills.sh) registry.
    #[default]
    SkillsSh,
}

impl std::fmt::Display for RegistryBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SkillsSh => f.write_str("skills-sh"),
        }
    }
}

fn default_registry_timeout_secs() -> u64 {
    30
}

/// External skill/plugin registry connection settings, nested under `[skills.registry]` in
/// TOML (spec-045, #5869).
///
/// Registry search/install is strictly opt-in (NFR-001): `enabled` defaults to `false`, and no
/// field on this type is read — no network call is made — unless `enabled = true`.
///
/// # Example (TOML)
///
/// ```toml
/// [skills.registry]
/// enabled = true
/// backend_kind = "skills-sh"
/// auth_vault_key = "ZEPH_SKILL_REGISTRY_TOKEN"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RegistryConfig {
    /// Enable registry search/install. Default: `false`.
    ///
    /// When `false`, `zeph skill search`/`add` and `zeph plugin search`/`add` refuse to make
    /// any network call and print an actionable opt-in message instead (FR-004).
    #[serde(default)]
    pub enabled: bool,
    /// Which registry backend implementation (`zeph_plugins::marketplace::RegistryClient`,
    /// crate not depended on here — see [`RegistryBackendKind`]) to use.
    #[serde(default)]
    pub backend_kind: RegistryBackendKind,
    /// Registry base URL. `None` uses the backend's built-in default (for
    /// [`RegistryBackendKind::SkillsSh`], `https://www.skills.sh`).
    #[serde(default)]
    pub backend_url: Option<String>,
    /// Vault key name to resolve the registry's bearer credential from, e.g.
    /// `"ZEPH_SKILL_REGISTRY_TOKEN"`. `None` means an anonymous (unauthenticated) request is
    /// attempted; the backend may reject it if it requires a credential.
    ///
    /// Always resolved via `VaultProvider` — never stored as a plain config field.
    #[serde(default)]
    pub auth_vault_key: Option<String>,
    /// Per-request timeout in seconds for registry `search`/`fetch` HTTP calls.
    #[serde(default = "default_registry_timeout_secs")]
    pub registry_timeout_secs: u64,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend_kind: RegistryBackendKind::default(),
            backend_url: None,
            auth_vault_key: None,
            registry_timeout_secs: default_registry_timeout_secs(),
        }
    }
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
/// subagent_skill_token_budget = 12000
/// ```
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
#[derive(Debug, Deserialize, Serialize)]
pub struct SkillsConfig {
    /// Directories to scan for `*.skill.md` / `SKILL.md` files.
    #[serde(default = "default_skill_paths")]
    pub paths: Vec<String>,
    #[serde(default = "default_max_active_skills")]
    pub max_active_skills: NonZeroUsize,
    #[serde(default = "default_disambiguation_threshold")]
    pub disambiguation_threshold: f32,
    #[serde(default = "default_min_injection_score")]
    pub min_injection_score: f32,
    #[serde(default = "default_cosine_weight")]
    pub cosine_weight: f32,
    #[serde(default = "default_hybrid_search")]
    pub hybrid_search: bool,
    /// Blend weight for BM25 hybrid retrieval: `score = bm25_alpha * cosine_clamped + (1 - bm25_alpha) * bm25_norm`.
    ///
    /// Only used when `hybrid_search = true`. Valid range: `[0.0, 1.0]`. Values outside this
    /// range are clamped at load time with a warning. Default: `0.7` (cosine-dominant).
    #[serde(default = "default_bm25_alpha")]
    pub bm25_alpha: f32,
    #[serde(default)]
    pub learning: LearningConfig,
    #[serde(default)]
    pub trust: TrustConfig,
    /// External skill/plugin registry discovery (`zeph skill search`/`add`,
    /// `zeph plugin search`/`add`), nested under `[skills.registry]` in TOML (spec-045, #5869).
    ///
    /// Off by default: no network call is ever made to a registry unless `enabled = true`
    /// (NFR-001).
    #[serde(default)]
    pub registry: RegistryConfig,
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

    // --- Query rewriting ---
    /// Provider name for optional query rewriting before skill matching.
    ///
    /// When set to a non-empty provider name, the query is rewritten via a fast LLM call
    /// (5 s timeout) before embedding. The rewritten query is used only for skill matching,
    /// not for the conversation. When empty (default), query rewriting is disabled and the
    /// raw user query is embedded directly — zero overhead.
    #[serde(default)]
    pub query_rewrite_provider: ProviderName,

    // --- NL skill generation ---
    /// Provider name for `/skill create` NL generation. Empty = primary provider.
    #[serde(default)]
    pub generation_provider: ProviderName,
    /// Timeout in milliseconds for `/skill create` LLM generation. For `/skill create` this is
    /// enforced as a single end-to-end budget covering the initial call and its retry. The
    /// background promotion path (`GeneratorSkillWriter`) reuses the same value as a per-call
    /// budget instead (via `SkillGenerator::with_generation_timeout_ms`), so a generate-with-retry
    /// there may take up to 2x this value. Default: `60000` (60 s).
    #[serde(default = "default_generation_timeout_ms")]
    pub generation_timeout_ms: u64,
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
    /// Provider name for skill disambiguation LLM classification calls.
    ///
    /// When set, the named provider is used instead of the primary provider for
    /// skill disambiguation. Useful to route disambiguation to a cheaper or faster
    /// model. When empty (the default), the primary provider is used.
    #[serde(default)]
    pub disambiguate_provider: ProviderName,

    /// Enable LLM-backed semantic SKILL.md compliance scan on `plugin add`.
    ///
    /// When `true`, the agent asks an LLM whether the skill's declared purpose is
    /// consistent with its actual content. Non-compliant skills are rejected with a
    /// user-facing error message. `PluginError::SemanticViolation` is used only by the
    /// Stage-1 ephemeral path. Stage-1 regex scan always runs and is advisory regardless
    /// of this setting.
    ///
    /// Default: `false`.
    #[serde(default)]
    pub semantic_scan: bool,

    /// Provider name (from `[[llm.providers]]`) used for the semantic scan.
    ///
    /// When empty (the default), the primary/main provider is used.
    #[serde(default)]
    pub semantic_scan_provider: ProviderName,

    /// Enable `GoSkills` group-structured skill injection.
    ///
    /// When `true`, the top-N matched skills are presented to the LLM as an
    /// entry-point + support structure, improving multi-skill task execution.
    /// Falls back to flat injection when no pair exceeds `support_similarity_threshold`.
    ///
    /// Default: `false`.
    #[serde(default)]
    pub group_structured: bool,

    /// Inter-skill cosine similarity threshold for `GoSkills` grouping.
    ///
    /// A candidate skill becomes a support skill when its cosine similarity to the
    /// entry point exceeds this value (strict `>`). Valid range: `[0.0, 1.0]`.
    ///
    /// Default: `0.50`.
    #[serde(default = "default_support_similarity_threshold")]
    pub support_similarity_threshold: f32,

    /// Token budget for skill bodies injected into a sub-agent's one-shot system prompt.
    ///
    /// Sub-agent definitions with an empty `skills.include` filter inherit every skill in
    /// the registry (documented, intentional — see [`crate::SkillFilter`]). Unlike the main
    /// agent's per-turn skill matcher, a sub-agent's skill bodies are injected once, at spawn
    /// time, with no relevance ranking and no later opportunity to trim: an unbounded include
    /// set can silently blow the turn-1 context budget (#6421). This budget applies **only** to
    /// that empty-`include` case — a definition with an explicit, hand-curated `include` list is
    /// never capped, since the operator opted into that specific set on purpose. Skill bodies are
    /// greedily packed in registry order (alphabetical by skill directory, not relevance-ranked)
    /// up to the budget — an over-budget skill is skipped, not a hard stop, so a smaller skill
    /// later in the order can still fit — and any skills left out are surfaced via a visible
    /// truncation marker rather than silently dropped.
    ///
    /// Default: `12000` tokens.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::num::NonZeroUsize;
    /// use zeph_config::Config;
    ///
    /// let config = Config::default();
    /// assert_eq!(
    ///     config.skills.subagent_skill_token_budget,
    ///     NonZeroUsize::new(12_000).unwrap()
    /// );
    /// ```
    #[serde(default = "default_subagent_skill_token_budget")]
    pub subagent_skill_token_budget: NonZeroUsize,
}

fn default_generation_timeout_ms() -> u64 {
    60_000
}

fn default_support_similarity_threshold() -> f32 {
    0.50
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

fn default_mining_merge_threshold() -> f32 {
    0.75
}

fn default_mining_merge_enabled() -> bool {
    true
}

/// Configuration for the automated skill mining pipeline (`zeph-skills-miner` binary).
#[derive(Debug, Deserialize, Serialize)]
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
    /// Minimum similarity to trigger a merge with the nearest skill during mining. Default: 0.75.
    ///
    /// Must be strictly less than `dedup_threshold`.
    #[serde(default = "default_mining_merge_threshold")]
    pub merge_threshold: f32,
    /// When `false`, the merge zone (`merge_threshold <= sim < dedup_threshold`) collapses to
    /// discard during mining instead of merging into the nearest skill. Default: `true`.
    #[serde(default = "default_mining_merge_enabled")]
    pub merge_enabled: bool,
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
    /// Timeout in milliseconds for each LLM skill generation call during mining. Default: `30000` (30 s).
    #[serde(default = "default_mining_generation_timeout_ms")]
    pub generation_timeout_ms: u64,
}

impl Default for SkillMiningConfig {
    fn default() -> Self {
        Self {
            queries: Vec::new(),
            max_repos_per_query: default_max_repos_per_query(),
            dedup_threshold: default_dedup_threshold(),
            merge_threshold: default_mining_merge_threshold(),
            merge_enabled: default_mining_merge_enabled(),
            output_dir: None,
            generation_provider: ProviderName::default(),
            embedding_provider: ProviderName::default(),
            rate_limit_rpm: default_rate_limit_rpm(),
            generation_timeout_ms: default_mining_generation_timeout_ms(),
        }
    }
}

fn default_mining_generation_timeout_ms() -> u64 {
    30_000
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
#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
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
    /// Bounds concurrent CPU-bound chunk-parse (tree-sitter) dispatches via an internal
    /// semaphore. Default: 2.
    ///
    /// Only reduces concurrency below whatever `embed_concurrency` separately admits into
    /// flight via `buffer_unordered` in `index_batch` — has no effect when set >=
    /// `embed_concurrency` (the shipped default for both is 2).
    #[serde(default = "default_index_concurrency")]
    pub concurrency: usize,
    /// Delay in milliseconds inserted after each memory batch during the *initial* full-repo
    /// indexing pass only (not applied to incremental single-file reindex via the file
    /// watcher). Spreads CPU-bound chunk parsing over more wall-clock time so an interactive
    /// agent turn isn't starved for OS threads on large workspaces. Default: 75.
    #[serde(default = "default_initial_pass_batch_delay_ms")]
    pub initial_pass_batch_delay_ms: u64,
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
    /// at the API server level (rate limits, Ollama single-model lock). Falls back to the main
    /// agent provider when `None`.
    #[serde(default)]
    pub embedding_provider: Option<ProviderName>,
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
            initial_pass_batch_delay_ms: default_initial_pass_batch_delay_ms(),
            batch_size: default_index_batch_size(),
            memory_batch_size: default_index_memory_batch_size(),
            max_file_bytes: default_index_max_file_bytes(),
            embedding_provider: None,
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
    /// Which backend resolves secrets. Default: [`VaultBackend::Age`].
    #[serde(default = "default_vault_backend")]
    pub backend: VaultBackend,
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
/// max_daily_cents = 2500  # $25.00 per day (the default)
/// ```
#[derive(Debug, Deserialize, Serialize)]
pub struct CostConfig {
    /// Track and display token costs. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Daily spending cap in US cents (`0` = unlimited). Default: `2500` ($25.00/day).
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
/// webhook_send_timeout_secs = 5
/// ```
#[derive(Clone, Deserialize, Serialize)]
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
    ///
    /// # Security
    ///
    /// Never serialized: `--init` has no wizard path that persists this field (the real
    /// token is resolved from the vault via `ZEPH_GATEWAY_TOKEN`), but runtime config
    /// resolution hydrates the real value into this field in memory.
    /// `#[serde(skip_serializing)]` keeps any future diagnostic `Serialize` of a live
    /// `Config` from leaking it; `Deserialize` is untouched so an inline token in a
    /// hand-edited `config.toml` still loads.
    #[serde(default, skip_serializing)]
    pub auth_token: Option<String>,
    /// Maximum requests per minute. Must be `> 0`. Default: `120`.
    #[serde(default = "default_gateway_rate_limit")]
    pub rate_limit: u32,
    /// Maximum request body size in bytes. Must be `<= 10 MiB`. Default: `1048576` (1 MiB).
    #[serde(default = "default_gateway_max_body")]
    pub max_body_size: usize,
    /// Maximum seconds to wait for the agent to consume a webhook message before
    /// returning `503 Service Unavailable`. Default: `5`.
    #[serde(default = "default_gateway_webhook_send_timeout_secs")]
    pub webhook_send_timeout_secs: u64,
    /// CIDR ranges of trusted reverse proxies (e.g. `["10.0.0.0/8", "172.16.0.0/12"]`).
    ///
    /// When non-empty, the rate limiter applies the **rightmost-untrusted** algorithm on the
    /// `X-Forwarded-For` header: it walks the header from right to left and picks the first
    /// IP address that does NOT fall within any listed CIDR.  This is the correct algorithm
    /// when your proxy chain always appends, never prepends, so the rightmost entry added by
    /// the infrastructure is the one closest to your origin.
    ///
    /// Leave empty (the default) to use the raw TCP peer address for rate limiting, which is
    /// correct for deployments without a reverse proxy.
    ///
    /// Security note: only list CIDRs you fully control.  Any IP in a trusted CIDR can forge
    /// `X-Forwarded-For` and bypass per-IP rate limiting.
    #[serde(default)]
    pub trusted_proxy_cidrs: Vec<String>,
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
            webhook_send_timeout_secs: default_gateway_webhook_send_timeout_secs(),
            trusted_proxy_cidrs: Vec::new(),
        }
    }
}

impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("enabled", &self.enabled)
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("rate_limit", &self.rate_limit)
            .field("max_body_size", &self.max_body_size)
            .field("webhook_send_timeout_secs", &self.webhook_send_timeout_secs)
            .field("trusted_proxy_cidrs", &self.trusted_proxy_cidrs)
            .finish()
    }
}

impl GatewayConfig {
    /// Validate gateway configuration values.
    ///
    /// # Errors
    ///
    /// Returns an error string when:
    /// - `webhook_send_timeout_secs` is `0` or exceeds `300`
    /// - `max_body_size` exceeds `10 MiB` (`10485760` bytes)
    /// - `rate_limit` is `0` (causes division-by-zero in the token-bucket rate limiter)
    #[must_use = "validation result must be checked"]
    pub fn validate(&self) -> Result<(), String> {
        if self.webhook_send_timeout_secs == 0 || self.webhook_send_timeout_secs > 300 {
            return Err("webhook_send_timeout_secs must be between 1 and 300".to_owned());
        }
        if self.max_body_size > 10 * 1024 * 1024 {
            return Err("max_body_size must be <= 10485760 (10 MiB)".to_owned());
        }
        if self.rate_limit == 0 {
            return Err("rate_limit must be > 0".to_owned());
        }
        Ok(())
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
    /// Maximum seconds a task handler may run before being forcibly cancelled.
    /// Default: `300`. Set to `0` to disable the timeout.
    #[serde(default = "default_scheduler_handler_timeout_secs")]
    pub handler_timeout_secs: u64,
}

impl Default for SchedulerDaemonConfig {
    fn default() -> Self {
        Self {
            pid_file: default_scheduler_daemon_pid_file(),
            log_file: default_scheduler_daemon_log_file(),
            catch_up: true,
            tick_secs: default_scheduler_daemon_tick_secs(),
            shutdown_grace_secs: default_scheduler_daemon_shutdown_grace_secs(),
            handler_timeout_secs: default_scheduler_handler_timeout_secs(),
        }
    }
}

/// RTW-A temporal re-entry defense configuration for the scheduler.
///
/// Controls the four RTW-A mechanisms that protect the scheduler tick boundary
/// from prompt-injection attacks originating from the database.
///
/// # Example (TOML)
///
/// ```toml
/// [scheduler.security]
/// enabled = true
/// injection_pattern_check = true
/// attenuate_after_external_read = true
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerSecurityConfig {
    /// Enable all RTW-A re-entry defense mechanisms. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Mechanism 3: scan `task_data` for injection patterns before forwarding to the LLM.
    ///
    /// When enabled, prompts matching known injection markers are blocked and a
    /// `SchedulerError::PromptInjectionBlocked` is emitted.
    /// Default: `true`.
    #[serde(default = "default_true")]
    pub injection_pattern_check: bool,

    /// Mechanism 4: suppress `custom_task_tx` prompt injection after an external-read tick.
    ///
    /// When enabled, any tick that includes an `UpdateCheck` (or future network-reading)
    /// handler will not forward custom task prompts to the agent loop for that tick.
    /// Default: `true`.
    #[serde(default = "default_true")]
    pub attenuate_after_external_read: bool,
}

impl Default for SchedulerSecurityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            injection_pattern_check: true,
            attenuate_after_external_read: true,
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
/// kind = "custom"
/// config = { prompt = "Summarize what was accomplished today." }
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
    /// RTW-A re-entry defense settings.
    #[serde(default)]
    pub security: SchedulerSecurityConfig,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tick_interval_secs: default_scheduler_tick_interval(),
            max_tasks: default_scheduler_max_tasks(),
            tasks: Vec::new(),
            daemon: SchedulerDaemonConfig::default(),
            security: SchedulerSecurityConfig::default(),
        }
    }
}

/// Task kind for scheduled tasks.
///
/// Known variants map to built-in handlers; `Custom` accommodates user-defined task types.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
        assert_eq!(cfg.concurrency, 2);
        assert_eq!(cfg.batch_size, 32);
        assert_eq!(cfg.initial_pass_batch_delay_ms, 75);
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
        assert_eq!(cfg.concurrency, 2);
        assert_eq!(cfg.batch_size, 32);
        assert_eq!(cfg.initial_pass_batch_delay_ms, 75);
    }

    #[test]
    fn index_config_workspace_root_none_by_default() {
        let cfg: IndexConfig = toml::from_str("enabled = false").unwrap();
        assert!(cfg.workspace_root.is_none());
    }

    #[test]
    fn gateway_validate_timeout_zero_is_err() {
        let cfg = GatewayConfig {
            webhook_send_timeout_secs: 0,
            ..GatewayConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn gateway_validate_timeout_over_limit_is_err() {
        let cfg = GatewayConfig {
            webhook_send_timeout_secs: 301,
            ..GatewayConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn gateway_validate_max_body_over_limit_is_err() {
        let cfg = GatewayConfig {
            max_body_size: 10 * 1024 * 1024 + 1,
            ..GatewayConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn gateway_validate_defaults_are_ok() {
        assert!(GatewayConfig::default().validate().is_ok());
    }

    #[test]
    fn gateway_validate_rate_limit_zero_is_err() {
        let cfg = GatewayConfig {
            rate_limit: 0,
            ..GatewayConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn gateway_config_debug_redacts_auth_token() {
        let cfg = GatewayConfig {
            auth_token: Some("sk-SUPERSECRET".to_owned()),
            ..GatewayConfig::default()
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("sk-SUPERSECRET"));
        assert!(dbg.contains("[REDACTED]"));
    }

    #[test]
    fn gateway_config_debug_none_auth_token() {
        let cfg = GatewayConfig::default();
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("[REDACTED]"));
        assert!(dbg.contains("auth_token: None"));
    }

    #[test]
    fn gateway_config_serialize_omits_auth_token() {
        let cfg = GatewayConfig {
            auth_token: Some("real-secret-value".into()),
            ..GatewayConfig::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("real-secret-value"));
        assert!(!json.contains("\"auth_token\""));
    }

    #[test]
    fn gateway_config_deserialize_missing_auth_token_as_none() {
        // `#[serde(skip_serializing)]` only affects the output side; this pins that
        // `skip_serializing` cannot break loading a config that never had the key (e.g. one
        // written before this fix, or hand-edited without it).
        let cfg: GatewayConfig = toml::from_str("").unwrap();
        assert!(cfg.auth_token.is_none());
    }

    #[test]
    fn scheduler_config_default_is_disabled() {
        let cfg = SchedulerConfig::default();
        assert!(
            !cfg.enabled,
            "scheduler must be opt-in (enabled = false by default)"
        );
    }

    // ── RegistryConfig tests (spec-045, #5869) ────────────────────────────

    #[test]
    fn registry_config_default_is_disabled() {
        // Highest-priority test per the architect handoff: the registry must be strictly
        // opt-in (NFR-001) — zero network calls unless explicitly enabled.
        let cfg = RegistryConfig::default();
        assert!(
            !cfg.enabled,
            "skill/plugin registry must be opt-in (enabled = false by default)"
        );
        assert_eq!(cfg.backend_kind, RegistryBackendKind::SkillsSh);
        assert!(cfg.backend_url.is_none());
        assert!(cfg.auth_vault_key.is_none());
        assert_eq!(cfg.registry_timeout_secs, 30);
    }

    #[test]
    fn registry_config_serde_roundtrip_with_defaults() {
        let cfg: RegistryConfig = toml::from_str("").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.backend_kind, RegistryBackendKind::SkillsSh);
    }

    #[test]
    fn registry_config_serde_roundtrip_explicit() {
        let toml = r#"
            enabled = true
            backend_kind = "skills-sh"
            backend_url = "https://example.internal"
            auth_vault_key = "ZEPH_SKILL_REGISTRY_TOKEN"
            registry_timeout_secs = 10
        "#;
        let cfg: RegistryConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.backend_url.as_deref(), Some("https://example.internal"));
        assert_eq!(
            cfg.auth_vault_key.as_deref(),
            Some("ZEPH_SKILL_REGISTRY_TOKEN")
        );
        assert_eq!(cfg.registry_timeout_secs, 10);
    }

    #[test]
    fn registry_backend_kind_display() {
        assert_eq!(RegistryBackendKind::SkillsSh.to_string(), "skills-sh");
    }

    // --- CostConfig defaults (issue #6469) ---

    #[test]
    fn cost_config_default_has_nonzero_daily_cap() {
        let cfg = CostConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_daily_cents, 2500);
    }

    #[test]
    fn cost_config_absent_section_picks_up_new_default() {
        let cfg: CostConfig = toml::from_str("").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_daily_cents, 2500);
    }

    #[test]
    fn cost_config_explicit_zero_is_preserved() {
        let cfg: CostConfig = toml::from_str("max_daily_cents = 0").unwrap();
        assert_eq!(
            cfg.max_daily_cents, 0,
            "explicit 0 must mean unlimited, not be overridden"
        );
    }

    #[test]
    fn cost_config_explicit_nonzero_is_preserved() {
        let cfg: CostConfig = toml::from_str("max_daily_cents = 500").unwrap();
        assert_eq!(cfg.max_daily_cents, 500);
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
    /// Include full raw base64 `MessagePart::Image` bytes in debug dumps instead of a
    /// redacted `<redacted image: ...>` marker (#6306).
    ///
    /// Default: `false`. Image payloads are redacted by default to avoid writing
    /// potentially large or sensitive binary data to disk on an opt-in debugging feature.
    /// Enable only when a developer explicitly needs full wire-payload fidelity for
    /// image-related debugging.
    #[serde(default)]
    pub include_raw_images: bool,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            output_dir: super::defaults::default_debug_output_dir(),
            format: crate::dump_format::DumpFormat::default(),
            include_raw_images: false,
            traces: TraceConfig::default(),
        }
    }
}

/// Output style configuration for caveman ultra-compressed mode (`[caveman]`).
///
/// When `default_on = true` every new session starts in caveman mode. The mode can also be
/// toggled at runtime via the `/caveman` command or activated by the bundled `caveman` skill.
///
/// All fields have `#[serde(default)]` so existing configs parse without changes.
///
/// # Examples
///
/// ```
/// use zeph_config::CavemanConfig;
/// let cfg = CavemanConfig::default();
/// assert!(!cfg.default_on);
/// ```
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CavemanConfig {
    /// Start every session in ultra-compressed (telegraphic) output mode.
    ///
    /// Default: `false` (opt-in). Can be toggled at runtime with `/caveman [on|off]`.
    // TODO(critic): style knobs deferred — see #4985 MVP scope
    #[serde(default)]
    pub default_on: bool,
}
