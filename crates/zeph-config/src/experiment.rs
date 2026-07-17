// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;
use std::str::FromStr;

use crate::providers::ProviderName;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Sensitivity level of an asset accessed by an orchestrated task.
///
/// Set per-task on `TaskNode::asset_sensitivity` and graph-wide via
/// [`OrchestrationConfig::default_asset_sensitivity`].  In the current
/// implementation this is **advisory only** — the dispatcher does not yet
/// auto-restrict the tool allow-list based on this field.
/// See `specs/069-threat-model/spec.md §5` for enforcement caveats.
///
/// # Examples
///
/// ```rust
/// use zeph_config::AssetSensitivity;
///
/// assert_eq!(AssetSensitivity::default(), AssetSensitivity::Public);
/// let s: AssetSensitivity = serde_json::from_str("\"confidential\"").unwrap();
/// assert_eq!(s, AssetSensitivity::Confidential);
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssetSensitivity {
    /// No sensitive assets accessed (default).
    #[default]
    Public,
    /// Sensitive but not secret: user data, conversation history, semantic memory.
    Internal,
    /// Highly sensitive: vault keys, API credentials, private tokens.
    Confidential,
}

/// Strategy applied when a task in the orchestration graph fails.
///
/// Set at the graph level via [`OrchestrationConfig::default_failure_strategy`] and overridden
/// per-task in the task node. Variants map directly to the `serde` lowercase string form used in
/// TOML config and LLM-produced JSON plans.
///
/// # Examples
///
/// ```rust
/// use zeph_config::FailureStrategy;
///
/// assert_eq!(FailureStrategy::default(), FailureStrategy::Abort);
///
/// let s: FailureStrategy = serde_json::from_str("\"skip\"").unwrap();
/// assert_eq!(s, FailureStrategy::Skip);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FailureStrategy {
    /// Abort the entire graph and cancel all running tasks.
    #[default]
    Abort,
    /// Retry the task up to the configured `max_retries` limit, then abort.
    Retry,
    /// Skip the failed task and transitively skip all its dependents.
    Skip,
    /// Pause the graph and wait for user intervention.
    Ask,
}

impl fmt::Display for FailureStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Abort => write!(f, "abort"),
            Self::Retry => write!(f, "retry"),
            Self::Skip => write!(f, "skip"),
            Self::Ask => write!(f, "ask"),
        }
    }
}

impl FromStr for FailureStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "abort" => Ok(Self::Abort),
            "retry" => Ok(Self::Retry),
            "skip" => Ok(Self::Skip),
            "ask" => Ok(Self::Ask),
            other => Err(format!(
                "unknown failure strategy '{other}': expected one of abort, retry, skip, ask"
            )),
        }
    }
}

fn default_planner_max_tokens() -> u32 {
    4096
}

fn default_aggregator_max_tokens() -> u32 {
    4096
}

fn default_deferral_backoff_ms() -> u64 {
    100
}

fn default_experiment_max_experiments() -> u32 {
    20
}

fn default_experiment_max_wall_time_secs() -> u64 {
    3600
}

fn default_experiment_min_improvement() -> f64 {
    0.5
}

fn default_experiment_eval_budget_tokens() -> u64 {
    100_000
}

fn default_experiment_schedule_cron() -> String {
    "0 3 * * *".to_string()
}

fn default_experiment_max_experiments_per_run() -> u32 {
    20
}

fn default_experiment_schedule_max_wall_time_secs() -> u64 {
    1800
}

fn default_verify_max_tokens() -> u32 {
    1024
}

fn default_max_replans() -> u32 {
    2
}

fn default_completeness_threshold() -> f32 {
    0.7
}

fn default_cascade_failure_threshold() -> f32 {
    0.5
}

fn default_cascade_chain_threshold() -> usize {
    3
}

fn default_lineage_ttl_secs() -> u64 {
    300
}

fn default_max_predicate_replans() -> u32 {
    2
}

fn default_predicate_timeout_secs() -> u64 {
    30
}

fn default_persistence_enabled() -> bool {
    true
}

fn default_aggregator_timeout_secs() -> u64 {
    60
}

fn default_planner_timeout_secs() -> u64 {
    120
}

fn default_verifier_timeout_secs() -> u64 {
    120
}

fn default_ensemble_ema_alpha() -> f64 {
    0.3
}

fn default_ensemble_ema_decay() -> f64 {
    0.95
}

fn default_ensemble_min_observations() -> u32 {
    5
}

fn default_plan_cache_similarity_threshold() -> f32 {
    0.90
}

fn default_plan_cache_ttl_days() -> u32 {
    30
}

fn default_plan_cache_max_templates() -> u32 {
    100
}

/// Configuration for plan template caching (`[orchestration.plan_cache]` TOML section).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PlanCacheConfig {
    /// Enable plan template caching. Default: false.
    pub enabled: bool,
    /// Minimum cosine similarity to consider a cached template a match. Default: 0.90.
    #[serde(default = "default_plan_cache_similarity_threshold")]
    pub similarity_threshold: f32,
    /// Days since last access before a template is evicted. Default: 30.
    #[serde(default = "default_plan_cache_ttl_days")]
    pub ttl_days: u32,
    /// Maximum number of cached templates. Default: 100.
    #[serde(default = "default_plan_cache_max_templates")]
    pub max_templates: u32,
}

impl Default for PlanCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            similarity_threshold: default_plan_cache_similarity_threshold(),
            ttl_days: default_plan_cache_ttl_days(),
            max_templates: default_plan_cache_max_templates(),
        }
    }
}

impl PlanCacheConfig {
    /// Validate that all fields are within sane operating limits.
    ///
    /// # Errors
    ///
    /// Returns a description string if any field is outside the allowed range.
    #[must_use = "validation result must be checked"]
    pub fn validate(&self) -> Result<(), String> {
        if !(0.5..=1.0).contains(&self.similarity_threshold) {
            return Err(format!(
                "plan_cache.similarity_threshold must be in [0.5, 1.0], got {}",
                self.similarity_threshold
            ));
        }
        if self.max_templates == 0 || self.max_templates > 10_000 {
            return Err(format!(
                "plan_cache.max_templates must be in [1, 10000], got {}",
                self.max_templates
            ));
        }
        if self.ttl_days == 0 || self.ttl_days > 365 {
            return Err(format!(
                "plan_cache.ttl_days must be in [1, 365], got {}",
                self.ttl_days
            ));
        }
        Ok(())
    }
}

/// Configuration for ORCH-style deterministic verifier ensemble-merge
/// (`[orchestration.ensemble]` TOML section, spec `073-orch-ensemble-merge`).
///
/// Opt-in and default-`OFF`: when `enabled = false` (the default), `PlanVerifier::verify()`
/// behaves byte-for-byte identically to the single-provider path — no ensemble code runs.
///
/// `size` and `min_quorum` are intentionally NOT configurable fields — both are always
/// derived from `members.len()` (`quorum = members.len() / 2 + 1`) so they can never drift
/// out of sync with the member list.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EnsembleConfig {
    /// Enable ensemble machinery (member resolution at bootstrap). Default: `false`.
    ///
    /// Separate from `verify` so the ensemble can be resolved/warmed without yet being used
    /// for verification decisions.
    pub enabled: bool,
    /// Use the ensemble for `PlanVerifier` per-task verification. Default: `false`.
    ///
    /// Requires `enabled = true`. When `false` (even with `enabled = true`), the
    /// `SchedulerAction::Verify` handler still uses the single-provider `verify_provider`
    /// path — `verify` is the per-target activation flag.
    pub verify: bool,
    /// Provider names from `[[llm.providers]]`, one per ensemble member.
    ///
    /// Validated at config load time (when `enabled && verify`) to be odd-length, `>= 3`,
    /// and free of duplicates — this guarantees a strict majority with no ties by
    /// construction. Default: empty.
    pub members: Vec<String>,
    /// EMA smoothing factor for `EnsembleTracker`'s per-member agreement score, in `[0.0, 1.0]`.
    /// Higher values weight the most recent observation more heavily. Default: `0.3`.
    #[serde(default = "default_ensemble_ema_alpha")]
    pub ema_alpha: f64,
    /// Decay-toward-neutral-prior factor for `EnsembleTracker`, in `[0.0, 1.0]`. Default: `0.95`.
    #[serde(default = "default_ensemble_ema_decay")]
    pub ema_decay: f64,
    /// Minimum recorded observations before `EnsembleTracker::ema()` returns a score instead
    /// of `None` (cold-start gate). Default: `5`.
    #[serde(default = "default_ensemble_min_observations")]
    pub min_observations: u32,
    /// Per-member `chat_typed` call timeout in seconds. `0` = fall back to
    /// `verifier_timeout_secs`. Default: `0`.
    #[serde(default)]
    pub member_timeout_secs: u64,
}

impl Default for EnsembleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            verify: false,
            members: Vec::new(),
            ema_alpha: default_ensemble_ema_alpha(),
            ema_decay: default_ensemble_ema_decay(),
            min_observations: default_ensemble_min_observations(),
            member_timeout_secs: 0,
        }
    }
}

/// Configuration for Command-style dynamic task handoff
/// (`[orchestration.command]` TOML section, spec `080-cross-thread-store-dynamic-handoff`,
/// GitHub #6363).
///
/// Opt-in and default-`OFF`: when `enabled = false` (the default), a node's trailing
/// ` ```zeph-command ` output block is left as ordinary output text and
/// `TaskOutcome::Handoff` is never produced — zero behavior change (FR-B-001).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CommandConfig {
    /// Enable node-agent-driven dynamic task handoff via a trailing ` ```zeph-command `
    /// output block. Default: `false`.
    pub enabled: bool,
    /// Per-graph livelock budget: maximum number of `Command.goto` handoffs allowed across
    /// a single graph run (`dag::try_handoff`'s budget check). Validated `> 0` at config
    /// load time (FR-B-013) — `0` would make every handoff attempt fail immediately, which
    /// is indistinguishable from `enabled = false` but without the honest signal. Default:
    /// `16`.
    #[serde(default = "default_command_max_handoffs")]
    pub max_handoffs: u32,
}

impl Default for CommandConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_handoffs: default_command_max_handoffs(),
        }
    }
}

fn default_command_max_handoffs() -> u32 {
    16
}

/// Configuration for the task orchestration subsystem (`[orchestration]` TOML section).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
pub struct OrchestrationConfig {
    /// Enable the orchestration subsystem.
    pub enabled: bool,
    /// Maximum number of tasks in a single graph.
    pub max_tasks: u32,
    /// Maximum number of tasks that can run in parallel.
    pub max_parallel: u32,
    /// Default failure strategy applied to every task graph unless overridden per-task.
    #[serde(default)]
    pub default_failure_strategy: FailureStrategy,
    /// Default number of retries for the `retry` failure strategy.
    pub default_max_retries: u32,
    /// Timeout in seconds for a single task. `0` means no timeout.
    pub task_timeout_secs: u64,
    /// Provider name from `[[llm.providers]]` for planning LLM calls.
    /// Empty string = use the agent's primary provider.
    #[serde(default)]
    pub planner_provider: ProviderName,
    /// Maximum tokens budget hint for planner responses. Reserved for future use when
    /// per-call token limits are added to the `LlmProvider::chat` API.
    #[serde(default = "default_planner_max_tokens")]
    pub planner_max_tokens: u32,
    /// Total character budget for cross-task dependency context injection.
    pub dependency_context_budget: usize,
    /// Whether to show a confirmation prompt before executing a plan.
    pub confirm_before_execute: bool,
    /// Maximum tokens budget for aggregation LLM calls. Default: 4096.
    #[serde(default = "default_aggregator_max_tokens")]
    pub aggregator_max_tokens: u32,
    /// Base backoff for `ConcurrencyLimit` retries; grows exponentially (×2 each attempt) up to 5 s.
    #[serde(default = "default_deferral_backoff_ms")]
    pub deferral_backoff_ms: u64,
    /// Plan template caching configuration.
    #[serde(default)]
    pub plan_cache: PlanCacheConfig,
    /// Enable topology-aware concurrency selection. When true, `TopologyClassifier`
    /// adjusts `max_parallel` based on the DAG structure. Default: false (opt-in).
    #[serde(default)]
    pub topology_selection: bool,
    /// Provider name from `[[llm.providers]]` for verification LLM calls.
    /// Empty string = use the agent's primary provider. Should be a cheap/fast provider.
    #[serde(default)]
    pub verify_provider: ProviderName,
    /// Maximum tokens budget for verification LLM calls. Default: 1024.
    #[serde(default = "default_verify_max_tokens")]
    pub verify_max_tokens: u32,
    /// Maximum number of replan cycles per graph execution. Default: 2.
    ///
    /// Prevents infinite verify-replan loops. 0 = disable replan (verification still
    /// runs, gaps are logged only).
    #[serde(default = "default_max_replans")]
    pub max_replans: u32,
    /// Enable post-task completeness verification. Default: false (opt-in).
    ///
    /// When true, completed tasks are evaluated by `PlanVerifier`. Task stays
    /// `Completed` during verification; downstream tasks are unblocked immediately.
    /// Verification is best-effort and does not gate dispatch.
    #[serde(default)]
    pub verify_completeness: bool,
    /// Provider name from `[[llm.providers]]` for tool-dispatch routing.
    /// When set, tool-heavy tasks prefer this provider over the primary.
    /// Prefer mid-tier models (e.g., qwen2.5:14b) for reliability per arXiv:2601.16280.
    /// Empty string = use the primary provider.
    #[serde(default)]
    pub tool_provider: ProviderName,
    /// Minimum completeness score (0.0–1.0) for the plan to be accepted without
    /// replanning. Default: 0.7. When the verifier reports `confidence <
    /// completeness_threshold` AND gaps exist, a replan cycle is triggered.
    /// Used by both per-task and whole-plan verification.
    /// Values outside [0.0, 1.0] are rejected at startup by `Config::validate()`.
    #[serde(default = "default_completeness_threshold")]
    pub completeness_threshold: f32,
    /// Enable cascade-aware routing for Mixed-topology DAGs. Requires `topology_selection = true`.
    /// When enabled, tasks in failing subtrees are deprioritized in favour of healthy branches.
    /// Default: false (opt-in).
    #[serde(default)]
    pub cascade_routing: bool,
    /// Failure rate threshold (0.0–1.0) above which a DAG region is considered "cascading".
    /// Must be in (0.0, 1.0]. Default: 0.5.
    #[serde(default = "default_cascade_failure_threshold")]
    pub cascade_failure_threshold: f32,
    /// Enable tree-optimized dispatch for FanOut/FanIn topologies.
    /// Sorts the ready queue by critical-path distance (deepest tasks first) to minimize
    /// end-to-end latency. Default: false (opt-in).
    #[serde(default)]
    pub tree_optimized_dispatch: bool,

    /// `AdaptOrch` bandit-driven topology advisor. Default: disabled.
    #[serde(default)]
    pub adaptorch: AdaptOrchConfig,
    /// Consecutive-chain cascade abort threshold: number of consecutive `Failed` entries
    /// in a `depends_on` chain that triggers a DAG abort.
    ///
    /// `0` disables linear-chain cascade abort. Default: 3.
    /// Must not be `1` — a threshold of 1 would abort on every single failure.
    #[serde(default = "default_cascade_chain_threshold")]
    pub cascade_chain_threshold: usize,
    /// Fan-out cascade abort failure-rate threshold (0.0–1.0).
    ///
    /// When a DAG region's failure rate reaches this value AND the region has ≥ 3 tasks,
    /// the DAG is aborted immediately. `0.0` disables this signal (opt-in).
    /// Recommended production value: `0.7`.
    #[serde(default)]
    pub cascade_failure_rate_abort_threshold: f32,
    /// TTL for lineage entries in seconds. Entries older than this are pruned during
    /// chain merge. Setting this too low can prevent detection of slow-build cascades.
    ///
    /// Default: 300 seconds (5 minutes).
    #[serde(default = "default_lineage_ttl_secs")]
    pub lineage_ttl_secs: u64,
    /// Enable per-subtask predicate verification gate.
    ///
    /// Requires `predicate_provider` or a primary LLM provider to be configured.
    /// Default: false (opt-in).
    #[serde(default)]
    pub verify_predicate_enabled: bool,
    /// Provider name from `[[llm.providers]]` for predicate evaluation.
    ///
    /// Empty string = fall back to `verify_provider`, then primary.
    #[serde(default)]
    pub predicate_provider: ProviderName,
    /// Maximum number of predicate-driven task re-runs across the entire DAG.
    ///
    /// Independent of `max_replans` (verifier completeness budget). Default: 2.
    #[serde(default = "default_max_predicate_replans")]
    pub max_predicate_replans: u32,
    /// Timeout in seconds for each predicate LLM evaluation call.
    ///
    /// On timeout the evaluator returns a fail-open outcome (`passed = true`,
    /// `confidence = 0.0`) and logs a warning. Default: 30.
    #[serde(default = "default_predicate_timeout_secs")]
    pub predicate_timeout_secs: u64,
    /// Persist task graph state to `SQLite` across scheduler ticks.
    ///
    /// When `true` and a `SemanticMemory` store is available, the scheduler
    /// snapshots the graph once per tick and on plan completion. Graphs can
    /// then be rehydrated via `/plan resume <id>` after a restart.
    /// Default: `true`.
    #[serde(default = "default_persistence_enabled")]
    pub persistence_enabled: bool,
    /// Provider name from `[[llm.providers]]` for scheduling-tier LLM calls
    /// (aggregation, predicate evaluation, verification when no specific provider is set).
    ///
    /// Acts as fallback for `verify_provider` and `predicate_provider` when those are empty.
    /// Does NOT affect `planner_provider` — planning is a complex task and stays on the quality
    /// provider. Empty string = use the agent's primary provider.
    ///
    /// # Trade-off
    ///
    /// Setting this to a fast/cheap model reduces aggregation quality because `LlmAggregator`
    /// produces user-visible output. See CHANGELOG for details.
    #[serde(default)]
    pub orchestrator_provider: ProviderName,

    /// Default per-task cost budget in US cents. `0.0` = unlimited (no budget check).
    ///
    /// When a sub-agent task completes, the scheduler emits a `tracing::warn!` if the
    /// task exceeded this budget. In MVP this is **warn-only** — hard enforcement requires
    /// per-task `CostTracker` scoping, which is deferred post-v1.0.0.
    ///
    /// Individual tasks can override this via `TaskNode::token_budget_cents`.
    /// Default: `0.0` (unlimited).
    #[serde(default)]
    pub default_task_budget_cents: f64,

    /// Default asset sensitivity level for task nodes that do not set their own.
    ///
    /// Advisory only in the current implementation — the dispatcher does not yet
    /// auto-restrict tool access based on this field. See `specs/069-threat-model/spec.md §5`.
    ///
    /// TOML: `[orchestration] default_asset_sensitivity = "public"`
    /// Default: `public` (no restriction).
    #[serde(default)]
    pub default_asset_sensitivity: AssetSensitivity,

    /// Timeout in seconds for aggregation LLM calls. Default: 60.
    ///
    /// On timeout the aggregator falls back to raw concatenation so that a graph
    /// result is always returned. Set to `0` is rejected by `Config::validate()`.
    #[serde(default = "default_aggregator_timeout_secs")]
    pub aggregator_timeout_secs: u64,

    /// Timeout in seconds for planner LLM calls. Default: 120.
    ///
    /// On timeout the planner returns `OrchestrationError::PlanningFailed`.
    /// Planning has no fallback — without a graph no tasks can be dispatched.
    /// Set to `0` is rejected by `Config::validate()`.
    #[serde(default = "default_planner_timeout_secs")]
    pub planner_timeout_secs: u64,

    /// Timeout in seconds for verifier LLM calls (per-task and whole-plan). Default: 120.
    ///
    /// On timeout the verifier returns a fail-open result (`complete = true`, no gaps) and
    /// `ground()` is never called, so the entire tool-call grounding safety net (spec 009,
    /// #6278/#6287) silently never runs for that verification. Local Ollama models in the
    /// 20B+ parameter range (e.g. `gemma4:26b`) commonly take 60-120s to respond, so a low
    /// timeout here causes fail-open to trigger on essentially every verification when
    /// `verify_provider` targets such a model — see #6366.
    /// Set to `0` is rejected by `Config::validate()`.
    #[serde(default = "default_verifier_timeout_secs")]
    pub verifier_timeout_secs: u64,

    /// ORCH-style deterministic verifier ensemble-merge configuration. Default: disabled.
    /// See `specs/073-orch-ensemble-merge/spec.md`.
    #[serde(default)]
    pub ensemble: EnsembleConfig,

    /// Global default idle/no-progress timeout in seconds, used when a `TaskNode`'s own
    /// `TimeoutPolicy.idle_timeout_secs` is unset. `None` (the default) disables idle
    /// enforcement — it is opt-in, unlike `task_timeout_secs`.
    ///
    /// A task is killed if no progress heartbeat is observed for this many seconds — a
    /// heartbeat is written once per agent-loop turn boundary, so **this value must be set
    /// above the longest expected single-turn (single LLM call + its tool calls) duration**,
    /// or a healthy task performing one long-running tool call can be killed spuriously.
    /// Only enforced on the normal spawn dispatch path; `RunInline` tasks (no sub-agent
    /// definitions configured) are exempt. See
    /// `specs/075-orchestration-node-control-parity/spec.md` §4/FR-005.
    #[serde(default)]
    pub default_idle_timeout_secs: Option<u64>,

    /// Command-style dynamic task handoff configuration (spec-080, GitHub #6363). Default:
    /// disabled. See `specs/080-cross-thread-store-dynamic-handoff/spec.md`.
    #[serde(default)]
    pub command: CommandConfig,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_tasks: 20,
            max_parallel: 4,
            default_failure_strategy: FailureStrategy::default(),
            default_max_retries: 3,
            task_timeout_secs: 300,
            planner_provider: ProviderName::default(),
            planner_max_tokens: default_planner_max_tokens(),
            dependency_context_budget: 16384,
            confirm_before_execute: true,
            aggregator_max_tokens: default_aggregator_max_tokens(),
            deferral_backoff_ms: default_deferral_backoff_ms(),
            plan_cache: PlanCacheConfig::default(),
            topology_selection: false,
            verify_provider: ProviderName::default(),
            verify_max_tokens: default_verify_max_tokens(),
            max_replans: default_max_replans(),
            verify_completeness: false,
            completeness_threshold: default_completeness_threshold(),
            tool_provider: ProviderName::default(),
            cascade_routing: false,
            cascade_failure_threshold: default_cascade_failure_threshold(),
            tree_optimized_dispatch: false,
            adaptorch: AdaptOrchConfig::default(),
            cascade_chain_threshold: default_cascade_chain_threshold(),
            cascade_failure_rate_abort_threshold: 0.0,
            lineage_ttl_secs: default_lineage_ttl_secs(),
            verify_predicate_enabled: false,
            predicate_provider: ProviderName::default(),
            max_predicate_replans: default_max_predicate_replans(),
            predicate_timeout_secs: default_predicate_timeout_secs(),
            persistence_enabled: default_persistence_enabled(),
            orchestrator_provider: ProviderName::default(),
            default_task_budget_cents: 0.0,
            default_asset_sensitivity: AssetSensitivity::default(),
            aggregator_timeout_secs: default_aggregator_timeout_secs(),
            planner_timeout_secs: default_planner_timeout_secs(),
            verifier_timeout_secs: default_verifier_timeout_secs(),
            ensemble: EnsembleConfig::default(),
            default_idle_timeout_secs: None,
            command: CommandConfig::default(),
        }
    }
}

/// Configuration for the autonomous self-experimentation engine (`[experiments]` TOML section).
///
/// When `enabled = true`, Zeph periodically runs A/B experiments on its own skill and
/// prompt configurations to find improvements automatically.
///
/// # Example (TOML)
///
/// ```toml
/// [experiments]
/// enabled = false
/// max_experiments = 20
/// auto_apply = false
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ExperimentConfig {
    /// Enable autonomous self-experimentation. Default: `false`.
    pub enabled: bool,
    /// Provider name (from `[[llm.providers]]`) used as the LLM-as-judge for experiment
    /// evaluation. An empty value falls back to the primary provider. Prefer a capable,
    /// low-self-judge-bias model (e.g. a different provider than the one being evaluated).
    #[serde(default)]
    pub eval_provider: ProviderName,
    /// Path to a benchmark JSONL file for evaluating experiments.
    pub benchmark_file: Option<std::path::PathBuf>,
    #[serde(default = "default_experiment_max_experiments")]
    pub max_experiments: u32,
    #[serde(default = "default_experiment_max_wall_time_secs")]
    pub max_wall_time_secs: u64,
    #[serde(default = "default_experiment_min_improvement")]
    pub min_improvement: f64,
    #[serde(default = "default_experiment_eval_budget_tokens")]
    pub eval_budget_tokens: u64,
    pub auto_apply: bool,
    #[serde(default)]
    pub schedule: ExperimentSchedule,
    /// When `true`, a subject call failure (LLM error or timeout) excludes the case from
    /// scoring instead of aborting the entire evaluation run.
    ///
    /// Default: `false` (preserves existing abort-on-error semantics). Set to `true` when
    /// running parallel evaluations where a single subject timeout should not discard all
    /// already-billed responses from other in-flight futures — at the cost of producing a
    /// partial result rather than a guaranteed complete evaluation.
    #[serde(default)]
    pub tolerate_subject_errors: bool,
}

impl Default for ExperimentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            eval_provider: ProviderName::default(),
            benchmark_file: None,
            max_experiments: default_experiment_max_experiments(),
            max_wall_time_secs: default_experiment_max_wall_time_secs(),
            min_improvement: default_experiment_min_improvement(),
            eval_budget_tokens: default_experiment_eval_budget_tokens(),
            auto_apply: false,
            schedule: ExperimentSchedule::default(),
            tolerate_subject_errors: false,
        }
    }
}

/// Configuration for `AdaptOrch` — bandit-driven topology advisor (`[orchestration.adaptorch]`).
///
/// # Example
///
/// ```toml
/// [orchestration.adaptorch]
/// enabled = true
/// topology_provider = "fast"
/// classify_timeout_secs = 4
/// state_path = ""
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AdaptOrchConfig {
    /// Enable `AdaptOrch`. When `false`, planning uses the default `plan()` path.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for goal classification. Empty → primary provider.
    pub topology_provider: ProviderName,
    /// Hard timeout (seconds) for the classification LLM call.
    #[serde(default = "default_classify_timeout_secs")]
    pub classify_timeout_secs: u64,
    /// Path to the persisted Beta-arm JSON state file.
    /// Empty string → `~/.zeph/adaptorch_state.json` (resolved at runtime).
    #[serde(default)]
    pub state_path: String,
    /// Maximum tokens for the classification LLM call.
    #[serde(default = "default_max_classify_tokens")]
    pub max_classify_tokens: u32,
}

fn default_classify_timeout_secs() -> u64 {
    4
}

fn default_max_classify_tokens() -> u32 {
    80
}

impl Default for AdaptOrchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            topology_provider: ProviderName::default(),
            classify_timeout_secs: default_classify_timeout_secs(),
            state_path: String::new(),
            max_classify_tokens: default_max_classify_tokens(),
        }
    }
}

/// Cron scheduling configuration for automatic experiment runs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ExperimentSchedule {
    pub enabled: bool,
    #[serde(default = "default_experiment_schedule_cron")]
    pub cron: String,
    #[serde(default = "default_experiment_max_experiments_per_run")]
    pub max_experiments_per_run: u32,
    /// Wall-time cap for a single scheduled experiment session (seconds).
    ///
    /// Overrides `experiments.max_wall_time_secs` for scheduled runs. Defaults to 1800s so
    /// a background session cannot overlap the next cron trigger on typical schedules.
    #[serde(default = "default_experiment_schedule_max_wall_time_secs")]
    pub max_wall_time_secs: u64,
}

impl Default for ExperimentSchedule {
    fn default() -> Self {
        Self {
            enabled: false,
            cron: default_experiment_schedule_cron(),
            max_experiments_per_run: default_experiment_max_experiments_per_run(),
            max_wall_time_secs: default_experiment_schedule_max_wall_time_secs(),
        }
    }
}

impl ExperimentConfig {
    /// Validate that numeric bounds are within sane operating limits.
    ///
    /// # Errors
    ///
    /// Returns a description string if any field is outside allowed range.
    #[must_use = "validation result must be checked"]
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=1_000).contains(&self.max_experiments) {
            return Err(format!(
                "experiments.max_experiments must be in 1..=1000, got {}",
                self.max_experiments
            ));
        }
        if !(60..=86_400).contains(&self.max_wall_time_secs) {
            return Err(format!(
                "experiments.max_wall_time_secs must be in 60..=86400, got {}",
                self.max_wall_time_secs
            ));
        }
        if !(1_000..=10_000_000).contains(&self.eval_budget_tokens) {
            return Err(format!(
                "experiments.eval_budget_tokens must be in 1000..=10000000, got {}",
                self.eval_budget_tokens
            ));
        }
        if !(0.0..=100.0).contains(&self.min_improvement) {
            return Err(format!(
                "experiments.min_improvement must be in 0.0..=100.0, got {}",
                self.min_improvement
            ));
        }
        if !(1..=100).contains(&self.schedule.max_experiments_per_run) {
            return Err(format!(
                "experiments.schedule.max_experiments_per_run must be in 1..=100, got {}",
                self.schedule.max_experiments_per_run
            ));
        }
        if !(60..=86_400).contains(&self.schedule.max_wall_time_secs) {
            return Err(format!(
                "experiments.schedule.max_wall_time_secs must be in 60..=86400, got {}",
                self.schedule.max_wall_time_secs
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_cache_similarity_threshold_above_one_is_rejected() {
        let cfg = PlanCacheConfig {
            similarity_threshold: 1.1,
            ..PlanCacheConfig::default()
        };
        let result = cfg.validate();
        assert!(
            result.is_err(),
            "similarity_threshold = 1.1 must return a validation error"
        );
    }

    #[test]
    fn completeness_threshold_default_is_0_7() {
        let cfg = OrchestrationConfig::default();
        assert!(
            (cfg.completeness_threshold - 0.7).abs() < f32::EPSILON,
            "completeness_threshold default must be 0.7, got {}",
            cfg.completeness_threshold
        );
    }

    #[test]
    fn completeness_threshold_serde_round_trip() {
        let toml_in = r"
            enabled = true
            completeness_threshold = 0.85
        ";
        let cfg: OrchestrationConfig = toml::from_str(toml_in).expect("deserialize");
        assert!((cfg.completeness_threshold - 0.85).abs() < f32::EPSILON);

        let serialized = toml::to_string(&cfg).expect("serialize");
        let cfg2: OrchestrationConfig = toml::from_str(&serialized).expect("re-deserialize");
        assert!((cfg2.completeness_threshold - 0.85).abs() < f32::EPSILON);
    }

    #[test]
    fn completeness_threshold_missing_uses_default() {
        let toml_in = "enabled = true\n";
        let cfg: OrchestrationConfig = toml::from_str(toml_in).expect("deserialize");
        assert!(
            (cfg.completeness_threshold - 0.7).abs() < f32::EPSILON,
            "missing field must use default 0.7, got {}",
            cfg.completeness_threshold
        );
    }

    #[test]
    fn ensemble_config_default_is_disabled() {
        let cfg = EnsembleConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.verify);
        assert!(cfg.members.is_empty());
        assert!((cfg.ema_alpha - 0.3).abs() < f64::EPSILON);
        assert!((cfg.ema_decay - 0.95).abs() < f64::EPSILON);
        assert_eq!(cfg.min_observations, 5);
        assert_eq!(cfg.member_timeout_secs, 0);
    }

    #[test]
    fn orchestration_config_ensemble_is_disabled_by_default() {
        assert!(!OrchestrationConfig::default().ensemble.enabled);
    }

    #[test]
    fn ensemble_config_serde_round_trip() {
        let toml_in = r#"
            enabled = true
            verify = true
            members = ["fast", "quality", "cheap"]
            ema_alpha = 0.4
            ema_decay = 0.9
            min_observations = 10
            member_timeout_secs = 15
        "#;
        let cfg: EnsembleConfig = toml::from_str(toml_in).expect("deserialize");
        assert!(cfg.enabled);
        assert!(cfg.verify);
        assert_eq!(cfg.members, vec!["fast", "quality", "cheap"]);
        assert!((cfg.ema_alpha - 0.4).abs() < f64::EPSILON);

        let serialized = toml::to_string(&cfg).expect("serialize");
        let cfg2: EnsembleConfig = toml::from_str(&serialized).expect("re-deserialize");
        assert_eq!(cfg2.members, cfg.members);
        assert_eq!(cfg2.member_timeout_secs, 15);
    }

    #[test]
    fn ensemble_config_missing_section_uses_defaults() {
        // OrchestrationConfig has #[serde(default)] on the whole struct, and `ensemble` has
        // #[serde(default)] too — an existing config with no [orchestration.ensemble] table
        // at all must parse to the disabled default (no migration step required).
        let toml_in = "enabled = true\n";
        let cfg: OrchestrationConfig = toml::from_str(toml_in).expect("deserialize");
        assert!(!cfg.ensemble.enabled);
        assert!(cfg.ensemble.members.is_empty());
    }

    #[test]
    fn asset_sensitivity_default_is_public() {
        assert_eq!(AssetSensitivity::default(), AssetSensitivity::Public);
    }

    #[test]
    fn asset_sensitivity_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&AssetSensitivity::Public).unwrap(),
            "\"public\""
        );
        assert_eq!(
            serde_json::to_string(&AssetSensitivity::Confidential).unwrap(),
            "\"confidential\""
        );
        let v: AssetSensitivity = serde_json::from_str("\"internal\"").unwrap();
        assert_eq!(v, AssetSensitivity::Internal);
    }

    #[test]
    fn orchestration_config_default_asset_sensitivity_is_public() {
        let cfg = OrchestrationConfig::default();
        assert_eq!(cfg.default_asset_sensitivity, AssetSensitivity::Public);
    }

    #[test]
    fn orchestration_config_asset_sensitivity_toml_roundtrip() {
        let toml_in = "enabled = true\ndefault_asset_sensitivity = \"confidential\"\n";
        let cfg: OrchestrationConfig = toml::from_str(toml_in).expect("deserialize");
        assert_eq!(
            cfg.default_asset_sensitivity,
            AssetSensitivity::Confidential
        );
        let serialized = toml::to_string(&cfg).expect("serialize");
        let cfg2: OrchestrationConfig = toml::from_str(&serialized).expect("re-deserialize");
        assert_eq!(
            cfg2.default_asset_sensitivity,
            AssetSensitivity::Confidential
        );
    }

    #[test]
    fn orchestration_config_missing_asset_sensitivity_uses_default() {
        let toml_in = "enabled = true\n";
        let cfg: OrchestrationConfig = toml::from_str(toml_in).expect("deserialize");
        assert_eq!(cfg.default_asset_sensitivity, AssetSensitivity::Public);
    }

    // ── default_idle_timeout_secs (spec-075-orchestration-node-control-parity, #6021) ──

    #[test]
    fn orchestration_config_default_idle_timeout_secs_is_none() {
        let cfg = OrchestrationConfig::default();
        assert_eq!(cfg.default_idle_timeout_secs, None);
    }

    #[test]
    fn orchestration_config_idle_timeout_secs_toml_roundtrip() {
        let toml_in = "enabled = true\ndefault_idle_timeout_secs = 60\n";
        let cfg: OrchestrationConfig = toml::from_str(toml_in).expect("deserialize");
        assert_eq!(cfg.default_idle_timeout_secs, Some(60));
        let serialized = toml::to_string(&cfg).expect("serialize");
        let cfg2: OrchestrationConfig = toml::from_str(&serialized).expect("re-deserialize");
        assert_eq!(cfg2.default_idle_timeout_secs, Some(60));
    }

    #[test]
    fn orchestration_config_missing_idle_timeout_secs_migrates_to_none() {
        // Pre-feature config (no key present) — #[serde(default)] must yield None,
        // matching what a config persisted before this field existed deserializes to.
        let toml_in = "enabled = true\n";
        let cfg: OrchestrationConfig = toml::from_str(toml_in).expect("deserialize");
        assert_eq!(cfg.default_idle_timeout_secs, None);
    }

    #[test]
    fn orchestration_config_default_verifier_timeout_secs_is_120() {
        // #6366: the previous 30s default reliably timed out PlanVerifier::verify() against
        // 20B+ local Ollama models, causing fail-open to silently skip ground().
        assert_eq!(OrchestrationConfig::default().verifier_timeout_secs, 120);
    }
}
