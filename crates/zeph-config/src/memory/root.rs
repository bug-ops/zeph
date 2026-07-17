// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Root memory configuration and vector-store backend selection.
//!
//! Hosts [`MemoryConfig`] — the central `[memory]` table aggregating every memory
//! subsystem — and the [`VectorBackend`] selector for the embedding store.

use crate::defaults::{default_sqlite_path_field, default_true};
use crate::providers::ProviderName;
use serde::{Deserialize, Serialize};
use zeph_common::secret::Secret;

use super::{
    AdmissionConfig, AutoDreamConfig, CategoryConfig, CompressionConfig,
    CompressionGuidelinesConfig, ConsolidationConfig, ContextStrategy, CrossThreadStoreConfig,
    DigestConfig, DocumentConfig, EmGraphConfig, EpisodicConsolidationConfig, EvictionConfig,
    FiveSignalConfig, ForgettingConfig, GraphConfig, HebbianConfig, MemCotConfig,
    MicrocompactConfig, OpticalForgettingConfig, PersonaConfig, ReasoningConfig, RetrievalConfig,
    RetrievalFailuresConfig, SemanticConfig, SessionsConfig, SidequestConfig, StoreRoutingConfig,
    TierConfig, TieredRetrievalConfig, TrajectoryConfig, TrajectoryRiskAccumulatorConfig,
    TreeConfig, TypeAwareComposeConfig, WriteQualityGateConfig,
};

fn default_sqlite_pool_size() -> u32 {
    5
}

fn default_autosave_min_length() -> usize {
    20
}

fn default_tool_call_cutoff() -> usize {
    6
}

fn default_token_safety_margin() -> f32 {
    1.0
}

fn default_redact_credentials() -> bool {
    true
}

fn default_qdrant_url() -> String {
    "http://localhost:6334".into()
}

fn default_qdrant_timeout_secs() -> u64 {
    10
}

fn default_summarization_threshold() -> usize {
    50
}

fn default_summarization_llm_timeout_secs() -> u64 {
    60
}

fn default_context_budget_tokens() -> usize {
    0
}

fn default_soft_compaction_threshold() -> f32 {
    0.60
}

fn default_hard_compaction_threshold() -> f32 {
    0.90
}

fn default_compaction_preserve_tail() -> usize {
    6
}

fn default_compaction_cooldown_turns() -> u8 {
    2
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

fn default_shutdown_summary() -> bool {
    true
}

fn default_shutdown_summary_min_messages() -> usize {
    4
}

fn default_shutdown_summary_max_messages() -> usize {
    20
}

fn default_shutdown_summary_timeout_secs() -> u64 {
    30
}

/// Vector backend selector for embedding storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum VectorBackend {
    Qdrant,
    #[default]
    Sqlite,
}

impl VectorBackend {
    /// Return the lowercase identifier string for this backend.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::VectorBackend;
    ///
    /// assert_eq!(VectorBackend::Sqlite.as_str(), "sqlite");
    /// assert_eq!(VectorBackend::Qdrant.as_str(), "qdrant");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Qdrant => "qdrant",
            Self::Sqlite => "sqlite",
        }
    }
}

/// Memory subsystem configuration, nested under `[memory]` in TOML.
///
/// Controls `SQLite` and Qdrant storage, semantic recall, context compaction,
/// multi-tier promotion, and all memory-related background tasks.
///
/// # Example (TOML)
///
/// ```toml
/// [memory]
/// sqlite_path = "~/.local/share/zeph/data/zeph.db"
/// qdrant_url = "http://localhost:6334"
/// history_limit = 50
/// summarization_threshold = 50
/// auto_budget = true
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
pub struct MemoryConfig {
    #[serde(default)]
    pub compression_guidelines: CompressionGuidelinesConfig,
    #[serde(default = "default_sqlite_path_field")]
    pub sqlite_path: String,
    pub history_limit: u32,
    #[serde(default = "default_qdrant_url")]
    pub qdrant_url: String,
    /// Optional API key for authenticating to a remote or managed Qdrant cluster.
    ///
    /// Required when `qdrant_url` points to a non-localhost host (e.g. Qdrant Cloud).
    /// Leave `None` for local dev instances. The actual key is resolved from the vault:
    /// `zeph vault set ZEPH_QDRANT_API_KEY "<key>"`.
    ///
    /// The value is wrapped in [`Secret`] to prevent accidental logging.
    /// `skip_serializing` prevents the key from being written back to TOML on config save.
    #[serde(default, skip_serializing)]
    pub qdrant_api_key: Option<Secret>,
    /// Per-call timeout applied to every Qdrant gRPC operation, in seconds.
    ///
    /// Bounds each call against a hung server or a stalled network path instead of blocking
    /// the calling async task indefinitely. Default: `10`.
    #[serde(default = "default_qdrant_timeout_secs")]
    pub qdrant_timeout_secs: u64,
    #[serde(default)]
    pub semantic: SemanticConfig,
    #[serde(default = "default_summarization_threshold")]
    pub summarization_threshold: usize,
    /// LLM call timeout for summarization, in seconds. Default: `60`.
    #[serde(default = "default_summarization_llm_timeout_secs")]
    pub summarization_llm_timeout_secs: u64,
    #[serde(default = "default_context_budget_tokens")]
    pub context_budget_tokens: usize,
    #[serde(default = "default_soft_compaction_threshold")]
    pub soft_compaction_threshold: f32,
    #[serde(
        default = "default_hard_compaction_threshold",
        alias = "compaction_threshold"
    )]
    pub hard_compaction_threshold: f32,
    #[serde(default = "default_compaction_preserve_tail")]
    pub compaction_preserve_tail: usize,
    #[serde(default = "default_compaction_cooldown_turns")]
    pub compaction_cooldown_turns: u8,
    #[serde(default = "default_auto_budget")]
    pub auto_budget: bool,
    #[serde(default = "default_prune_protect_tokens")]
    pub prune_protect_tokens: usize,
    #[serde(default = "default_cross_session_score_threshold")]
    pub cross_session_score_threshold: f32,
    #[serde(default)]
    pub vector_backend: VectorBackend,
    #[serde(default = "default_token_safety_margin")]
    pub token_safety_margin: f32,
    #[serde(default = "default_redact_credentials")]
    pub redact_credentials: bool,
    #[serde(default = "default_true")]
    pub autosave_assistant: bool,
    #[serde(default = "default_autosave_min_length")]
    pub autosave_min_length: usize,
    #[serde(default = "default_tool_call_cutoff")]
    pub tool_call_cutoff: usize,
    #[serde(default = "default_sqlite_pool_size")]
    pub sqlite_pool_size: u32,
    #[serde(default)]
    pub sessions: SessionsConfig,
    #[serde(default)]
    pub documents: DocumentConfig,
    #[serde(default)]
    pub eviction: EvictionConfig,
    #[serde(default)]
    pub compression: CompressionConfig,
    #[serde(default)]
    pub sidequest: SidequestConfig,
    #[serde(default)]
    pub graph: GraphConfig,
    /// Store a lightweight session summary to the vector store on shutdown when no session
    /// summary exists yet for this conversation. Enables cross-session recall for short or
    /// interrupted sessions that never triggered hard compaction. Default: `true`.
    #[serde(default = "default_shutdown_summary")]
    pub shutdown_summary: bool,
    /// Minimum number of user-turn messages required before a shutdown summary is generated.
    /// Sessions below this threshold are considered trivial and skipped. Default: `4`.
    #[serde(default = "default_shutdown_summary_min_messages")]
    pub shutdown_summary_min_messages: usize,
    /// Maximum number of recent messages (user + assistant) sent to the LLM for shutdown
    /// summarization. Caps token cost for long sessions that never triggered hard compaction.
    /// Default: `20`.
    #[serde(default = "default_shutdown_summary_max_messages")]
    pub shutdown_summary_max_messages: usize,
    /// Per-attempt timeout in seconds for each LLM call during shutdown summarization.
    /// Applies independently to the structured call and to the plain-text fallback.
    /// Default: `10`.
    #[serde(default = "default_shutdown_summary_timeout_secs")]
    pub shutdown_summary_timeout_secs: u64,
    /// LLM provider used for shutdown summarization calls.
    ///
    /// Accepts a provider name from `[[llm.providers]]`. When empty, falls back to the primary
    /// provider. Use a fast, cost-efficient model (e.g. `"fast"`) to minimise shutdown latency.
    ///
    /// Example:
    /// ```toml
    /// [memory]
    /// shutdown_summary_provider = "fast"
    /// ```
    #[serde(default)]
    pub shutdown_summary_provider: ProviderName,
    /// LLM provider used for deferred tool-pair summarization (context compaction).
    ///
    /// Accepts a provider name from `[[llm.providers]]`. When empty, falls back to the primary
    /// provider. A mid-tier model is usually sufficient for compaction summaries.
    ///
    /// Example:
    /// ```toml
    /// [memory]
    /// compaction_provider = "fast"
    /// ```
    #[serde(default)]
    pub compaction_provider: ProviderName,
    /// Use structured anchored summaries for context compaction.
    ///
    /// When enabled, hard compaction requests a JSON schema from the LLM
    /// instead of free-form prose. Falls back to prose if the LLM fails
    /// to produce valid JSON. Default: `false`.
    #[serde(default)]
    pub structured_summaries: bool,
    /// AOI three-layer memory tier promotion system.
    ///
    /// When `tiers.enabled = true`, a background sweep promotes frequently-accessed episodic
    /// messages to a semantic tier by clustering near-duplicates and distilling via LLM.
    #[serde(default)]
    pub tiers: TierConfig,
    /// A-MAC adaptive memory admission control.
    ///
    /// When `admission.enabled = true`, each message is evaluated before saving and rejected
    /// if its composite admission score falls below the configured threshold.
    #[serde(default)]
    pub admission: AdmissionConfig,
    /// Session digest generation at session end. Default: disabled.
    #[serde(default)]
    pub digest: DigestConfig,
    /// Context assembly strategy. Default: `full_history` (current behavior).
    #[serde(default)]
    pub context_strategy: ContextStrategy,
    /// Number of turns at which `Adaptive` strategy switches to `MemoryFirst`. Default: `20`.
    #[serde(default = "default_crossover_turn_threshold")]
    pub crossover_turn_threshold: u32,
    /// All-Mem lifelong memory consolidation sweep.
    ///
    /// When `consolidation.enabled = true`, a background loop clusters semantically similar
    /// messages and merges them into consolidated entries via LLM.
    #[serde(default)]
    pub consolidation: ConsolidationConfig,
    /// `SleepGate` forgetting sweep (#2397).
    ///
    /// When `forgetting.enabled = true`, a background loop periodically decays importance
    /// scores and prunes memories below the forgetting floor.
    #[serde(default)]
    pub forgetting: ForgettingConfig,
    /// `PostgreSQL` connection URL.
    ///
    /// Used when the binary is compiled with `--features postgres`.
    /// Can be overridden by the vault key `ZEPH_DATABASE_URL`.
    /// Example: `postgres://user:pass@localhost:5432/zeph`
    /// Default: `None` (uses `sqlite_path` instead).
    ///
    /// The value is wrapped in [`Secret`] to prevent accidental logging — it commonly
    /// embeds a username and password. `skip_serializing` prevents it from being written
    /// back to TOML on config save, matching [`qdrant_api_key`](Self::qdrant_api_key).
    #[serde(default, skip_serializing)]
    pub database_url: Option<Secret>,
    /// Cost-sensitive store routing (#2444).
    ///
    /// When `store_routing.enabled = true`, query intent is classified and routed to
    /// the cheapest sufficient backend instead of querying all stores on every turn.
    #[serde(default)]
    pub store_routing: StoreRoutingConfig,
    /// Persona memory layer (#2461).
    ///
    /// When `persona.enabled = true`, user preferences and domain knowledge are extracted
    /// from conversation history and injected into context after the system prompt.
    #[serde(default)]
    pub persona: PersonaConfig,
    /// Trajectory-informed memory (#2498).
    #[serde(default)]
    pub trajectory: TrajectoryConfig,
    /// Category-aware memory (#2428).
    #[serde(default)]
    pub category: CategoryConfig,
    /// `TiMem` temporal-hierarchical memory tree (#2262).
    #[serde(default)]
    pub tree: TreeConfig,
    /// Time-based microcompact (#2699).
    ///
    /// When `microcompact.enabled = true`, stale low-value tool outputs are cleared
    /// from context when the session has been idle longer than `gap_threshold_minutes`.
    #[serde(default)]
    pub microcompact: MicrocompactConfig,
    /// autoDream background memory consolidation (#2697).
    ///
    /// When `autodream.enabled = true`, a constrained consolidation subagent runs
    /// after a session ends if both `min_sessions` and `min_hours` gates pass.
    #[serde(default)]
    pub autodream: AutoDreamConfig,
    /// Cosine similarity threshold for deduplicating key facts in `zeph_key_facts` (#2717).
    ///
    /// Before inserting a new key fact, its nearest neighbour is looked up in the
    /// `zeph_key_facts` collection.  If the best score is ≥ this threshold the fact is
    /// considered a near-duplicate and skipped.  Set to a value greater than `1.0` (e.g.
    /// `2.0`) to disable dedup entirely.  Default: `0.95`.
    #[serde(default = "default_key_facts_dedup_threshold")]
    pub key_facts_dedup_threshold: f32,
    /// Experience compression spectrum (#3305).
    ///
    /// Controls three-tier retrieval policy and background skill-promotion engine.
    #[serde(default)]
    pub compression_spectrum: crate::features::CompressionSpectrumConfig,
    /// MemMachine-inspired retrieval-stage tuning (#3340).
    ///
    /// Controls ANN candidate depth, search-prompt formatting, and the shape of memory snippets
    /// injected into agent context. Separate from `SemanticConfig` because these knobs apply
    /// uniformly across graph, hybrid, and vector-only recall paths.
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [memory.retrieval]
    /// depth = 40
    /// search_prompt_template = ""
    /// context_format = "structured"
    /// ```
    #[serde(default)]
    pub retrieval: RetrievalConfig,
    /// `ReasoningBank`: distilled reasoning strategy memory (#3342).
    ///
    /// When `reasoning.enabled = true`, each completed agent turn is evaluated by a self-judge
    /// LLM call; successful and failed reasoning chains are compressed into short, generalizable
    /// strategy summaries stored in `reasoning_strategies` (`SQLite`) and a matching Qdrant
    /// collection. Top-k strategies are retrieved by embedding similarity at context-build time
    /// and injected before the LLM call.
    #[serde(default)]
    pub reasoning: ReasoningConfig,
    /// Hebbian edge-weight reinforcement configuration (HL-F1/F2, #3344).
    ///
    /// When `enabled = true`, the weight of each `graph_edges` row is incremented
    /// by `hebbian_lr` every time that edge is traversed during a recall. Default: disabled.
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [memory.hebbian]
    /// enabled = true
    /// hebbian_lr = 0.1
    /// ```
    #[serde(default)]
    pub hebbian: HebbianConfig,
    /// `MemCoT` rolling semantic state configuration (#3574).
    ///
    /// When `enabled = true`, each completed assistant turn spawns a background distillation
    /// task that compresses the response into a short semantic state buffer. The buffer is
    /// prepended to graph recall queries so retrieval stays contextually relevant across long
    /// multi-turn sessions.
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [memory.memcot]
    /// enabled = true
    /// distill_provider = "fast"
    /// min_assistant_chars = 200
    /// max_distills_per_session = 50
    /// ```
    #[serde(default)]
    pub memcot: MemCotConfig,
    /// `OmniMem` retrieval failure tracking (issue #3576).
    ///
    /// When `enabled = true`, no-hit and low-confidence recall events are logged
    /// asynchronously to `memory_retrieval_failures` for closed-loop parameter tuning.
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [memory.retrieval_failures]
    /// enabled = true
    /// low_confidence_threshold = 0.3
    /// retention_days = 90
    /// ```
    #[serde(default)]
    pub retrieval_failures: RetrievalFailuresConfig,
    /// Write quality gate (#3629).
    ///
    /// When `quality_gate.enabled = true`, each `remember()` call is scored and low-quality
    /// writes are rejected before persistence. Evaluated after A-MAC admission control.
    #[serde(default)]
    pub quality_gate: WriteQualityGateConfig,
    /// `MemFlow` tiered intent-driven retrieval (issue #3712).
    ///
    /// When `tiered_retrieval.enabled = true`, recall queries are classified by intent and
    /// dispatched to the cheapest sufficient tier (`ProfileLookup` → `TargetedRetrieval` →
    /// `DeepReasoning`) with optional validation and tier escalation.
    #[serde(default)]
    pub tiered_retrieval: TieredRetrievalConfig,
    /// `MemGuard`-inspired type-aware retrieval composition (spec 004-16, issue #6086).
    ///
    /// When `type_aware_compose.enabled = true`, `schedule_context_fetchers` composes only the
    /// functional memory types in the active set instead of unconditionally injecting all of
    /// them. Retrieval-only: no new Qdrant collection, no write-path change. Default: disabled
    /// (byte-for-byte no-op).
    #[serde(default)]
    pub type_aware_compose: TypeAwareComposeConfig,
    /// `ScrapMem` optical forgetting (issue #3713).
    ///
    /// When `optical_forgetting.enabled = true`, a background sweep progressively compresses
    /// old messages: `Full` → `Compressed` → `SummaryOnly`, saving token budget in context assembly.
    #[serde(default)]
    pub optical_forgetting: OpticalForgettingConfig,
    /// EM-Graph episodic event extraction and causal linking (issue #3713).
    ///
    /// When `em_graph.enabled = true`, episodic events are extracted from conversation turns
    /// and linked via causal relationships, enabling causal-chain retrieval.
    #[serde(default)]
    pub em_graph: EmGraphConfig,
    /// Episodic-to-semantic consolidation daemon (issue #3799).
    ///
    /// When `episodic_consolidation.enabled = true`, a background loop periodically sweeps
    /// mature `episodic_events`, extracts durable facts via LLM, deduplicates against existing
    /// key facts, and promotes them to the semantic tier in `zeph_key_facts`.
    #[serde(default)]
    pub episodic_consolidation: EpisodicConsolidationConfig,
    /// MAGE shadow memory trajectory risk accumulator (spec 004-16).
    ///
    /// Maintains a per-session rolling risk score fed by sanitizer audit signals.
    /// When `shadow_memory.enabled = true`, tool execution is gated if cumulative
    /// trajectory risk exceeds `risk_threshold`. When `false`, all code paths are
    /// zero-cost no-ops.
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [memory.shadow_memory]
    /// enabled = true
    /// risk_threshold = 0.75
    /// risk_halflife_turns = 10
    /// ```
    #[serde(default)]
    pub shadow_memory: TrajectoryRiskAccumulatorConfig,
    /// Five-signal SYNAPSE retrieval (issue #4374).
    ///
    /// When `five_signal.enabled = true`, SYNAPSE recall weights five signals: recency,
    /// relevance, access frequency, causal distance, and novelty. All new signals default
    /// to weight `0.0`, preserving exact backward compatibility.
    #[serde(default)]
    pub five_signal: FiveSignalConfig,
    /// Context-Adaptive Memory fidelity scoring (CAM Phase 1, #4547).
    ///
    /// When `fidelity.enabled = true`, the heuristic fidelity scorer runs after each
    /// `apply_prepared_context()` call and assigns `Full / Compressed / Placeholder`
    /// levels to historical messages. Default: disabled.
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [memory.fidelity]
    /// enabled = false
    /// w_semantic = 0.3
    /// w_temporal = 0.3
    /// w_importance = 0.2
    /// w_plan = 0.2
    /// full_threshold = 0.7
    /// compressed_threshold = 0.3
    /// compressed_max_tokens = 50
    /// regrade_threshold = 0.6
    /// min_query_length = 8
    /// max_scored_messages = 500
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fidelity: Option<crate::fidelity::FidelityConfig>,
    /// Generic namespaced cross-thread key-value store (spec-080, #6363).
    ///
    /// When `store.enabled = true`, `zeph store {get,put,list,delete}` (CLI/slash command)
    /// and `zeph-orchestration`'s `Command.update` handoff (via `zeph-core`) can read and
    /// write rows addressed by `(owner_key, namespace, key)`. Default: disabled — zero
    /// behavior change (FR-A-001).
    #[serde(default)]
    pub store: CrossThreadStoreConfig,
}

fn default_crossover_turn_threshold() -> u32 {
    20
}

fn default_key_facts_dedup_threshold() -> f32 {
    0.95
}
