// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! High-level semantic memory orchestrator.
//!
//! [`SemanticMemory`] is the primary entry point used by `zeph-core`.  It wires
//! together [`crate::store::SqliteStore`] (relational persistence) and
//! [`crate::embedding_store::EmbeddingStore`] (Qdrant vector index) into a single
//! object with `remember` / `recall` / `summarize` operations.
//!
//! # Construction
//!
//! Use [`SemanticMemory::new`] for the default 0.7/0.3 vector/keyword weights, or
//! [`SemanticMemory::with_qdrant_ops`] inside `AppBuilder` to share a single gRPC
//! channel across all subsystems.
//!
//! # Hybrid recall
//!
//! Recall uses reciprocal-rank fusion of BM25 (`SQLite` FTS5) and cosine-similarity
//! (Qdrant) results, with optional temporal decay, MMR diversity reranking, and
//! per-tier score boosts.

mod algorithms;
mod corrections;
mod cross_session;
mod graph;
pub(crate) mod importance;
pub mod persona;
mod recall;
mod summarization;
pub mod trajectory;
pub mod tree_consolidation;
pub(crate) mod write_buffer;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use tokio::sync::RwLock;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;

use crate::admission::AdmissionControl;
use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::token_counter::TokenCounter;

pub(crate) const SESSION_SUMMARIES_COLLECTION: &str = "zeph_session_summaries";
pub(crate) const KEY_FACTS_COLLECTION: &str = "zeph_key_facts";
pub(crate) const CORRECTIONS_COLLECTION: &str = "zeph_corrections";

/// Progress state for embed backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillProgress {
    /// Number of messages processed so far (including failures).
    pub done: usize,
    /// Total number of unembedded messages at backfill start.
    pub total: usize,
}

pub use algorithms::{apply_mmr, apply_temporal_decay};
pub use cross_session::SessionSummaryResult;
pub use graph::{
    ExtractionResult, ExtractionStats, GraphExtractionConfig, LinkingStats, NoteLinkingConfig,
    PostExtractValidator, extract_and_store, link_memory_notes,
};
pub use persona::{
    PersonaExtractionConfig, contains_self_referential_language, extract_persona_facts,
};
pub use recall::{EmbedContext, RecalledMessage};
pub use summarization::{StructuredSummary, Summary, build_summarization_prompt};
pub use trajectory::{TrajectoryEntry, TrajectoryExtractionConfig, extract_trajectory_entries};
pub use tree_consolidation::{
    TreeConsolidationConfig, TreeConsolidationResult, run_tree_consolidation_sweep,
    start_tree_consolidation_loop,
};
pub use write_buffer::{BufferedWrite, WriteBuffer};

/// Cached profile centroid for query-bias correction (MM-F3, #3341).
///
/// Stored inside `SemanticMemory::profile_centroid` under an `RwLock`. Expires after
/// `profile_centroid_ttl_secs` seconds; a miss is non-sticky (next call retries).
#[derive(Debug, Clone)]
pub(crate) struct CachedCentroid {
    /// The centroid vector (unweighted mean of persona-fact embeddings).
    pub vector: Vec<f32>,
    /// Wall-clock instant when this centroid was computed.
    pub computed_at: Instant,
}

/// Classification of a user query's self-referential intent (MM-F3, #3341).
///
/// Used to decide whether query-bias correction should shift the embedding
/// towards the user's profile centroid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryIntent {
    /// Query contains first-person language — likely about the user themselves.
    FirstPerson,
    /// Query is about an external topic; no bias shift applied.
    Other,
}

/// HL-F5 runtime wiring for spreading activation (mirror of `[memory.hebbian]` spread fields).
///
/// Built from config at bootstrap and attached via [`SemanticMemory::with_hebbian_spread`].
#[derive(Debug, Clone, Default)]
pub struct HelaSpreadRuntime {
    /// `true` when `[memory.hebbian] enabled = true` AND `spreading_activation = true`.
    pub enabled: bool,
    /// BFS hops, already clamped to `[1, 6]` by the caller.
    pub depth: u32,
    /// Soft upper bound on the visited-node set.
    pub max_visited: usize,
    /// MAGMA edge-type filter for BFS traversal.
    pub edge_types: Vec<crate::graph::EdgeType>,
    /// Per-step circuit-breaker duration.
    pub step_budget: Option<std::time::Duration>,
}

/// High-level semantic memory orchestrator combining `SQLite` and Qdrant.
///
/// Instantiate via [`SemanticMemory::new`] or the `AppBuilder` integration.
/// All fields are `pub(crate)` — callers interact through the inherent method API.
// TODO(review): Refactor the five bool flags into two-variant enums to satisfy
// clippy::struct_excessive_bools. Left for a follow-up to avoid scope creep.
#[allow(clippy::struct_excessive_bools)] // independent boolean flags; bitflags or enum would obscure semantics without reducing complexity
pub struct SemanticMemory {
    pub(crate) sqlite: SqliteStore,
    pub(crate) qdrant: Option<Arc<EmbeddingStore>>,
    pub(crate) provider: AnyProvider,
    /// Dedicated provider for batch embedding calls (backfill, write-path embedding).
    ///
    /// When `Some`, all embedding I/O is routed through this provider instead of `provider`.
    /// This prevents `embed_backfill` from saturating the main provider and causing guardrail
    /// timeouts. When `None`, falls back to `provider`.
    pub(crate) embed_provider: Option<AnyProvider>,
    pub(crate) embedding_model: String,
    pub(crate) vector_weight: f64,
    pub(crate) keyword_weight: f64,
    pub(crate) temporal_decay_enabled: bool,
    pub(crate) temporal_decay_half_life_days: u32,
    pub(crate) mmr_enabled: bool,
    pub(crate) mmr_lambda: f32,
    pub(crate) importance_enabled: bool,
    pub(crate) importance_weight: f64,
    /// Multiplicative score boost for semantic-tier messages in recall ranking.
    /// Default: `1.3`. Disabled when set to `1.0`.
    pub(crate) tier_boost_semantic: f64,
    pub token_counter: Arc<TokenCounter>,
    pub graph_store: Option<Arc<crate::graph::GraphStore>>,
    /// Experience store for tool-outcome telemetry and per-turn evolution sweeps.
    ///
    /// `Some` when `memory.graph.experience.enabled = true` at bootstrap.
    pub experience: Option<Arc<crate::graph::experience::ExperienceStore>>,
    /// `ReasoningBank` store for distilled reasoning strategies (#3342).
    ///
    /// `Some` when `memory.reasoning.enabled = true` at bootstrap.
    pub reasoning: Option<Arc<crate::reasoning::ReasoningMemory>>,
    pub(crate) community_detection_failures: Arc<AtomicU64>,
    pub(crate) graph_extraction_count: Arc<AtomicU64>,
    pub(crate) graph_extraction_failures: Arc<AtomicU64>,
    pub(crate) last_qdrant_warn: Arc<AtomicU64>,
    /// A-MAC admission control gate. When `Some`, each `remember()` call is evaluated.
    pub(crate) admission_control: Option<Arc<AdmissionControl>>,
    /// Write quality gate. When `Some`, evaluated in `remember()`/`remember_with_parts()`
    /// after A-MAC admission and before persistence.
    pub(crate) quality_gate: Option<Arc<crate::quality_gate::QualityGate>>,
    /// Cosine similarity threshold for skipping near-duplicate key facts (0.0–1.0).
    /// When a new fact's nearest neighbour in `zeph_key_facts` has score >= this value,
    /// the fact is considered a duplicate and not inserted.  Default: `0.95`.
    pub(crate) key_facts_dedup_threshold: f32,
    /// Bounded set of in-flight background embed tasks.
    ///
    /// Guarded by a `Mutex` because `SemanticMemory` is shared via `Arc` and
    /// `JoinSet` requires `&mut self` for `spawn`. Capacity is capped at
    /// `MAX_EMBED_BG_TASKS`; tasks that exceed the limit are dropped with a debug log.
    pub(crate) embed_tasks: Mutex<tokio::task::JoinSet<()>>,
    /// ANN candidate count fetched from the vector store before reranking (MM-F1, #3340).
    ///
    /// `0` = legacy behavior (`recall_limit * 2`). `≥ 1` = direct count.
    pub(crate) retrieval_depth: u32,
    /// Template applied to raw user queries before embedding (MM-F2, #3340).
    ///
    /// Empty string = identity (pass raw query through). Applied at query-side embed sites only;
    /// never applied to stored content (summaries, documents).
    pub(crate) search_prompt_template: String,
    /// Fires `tracing::warn!` once per instance when `retrieval_depth < recall_limit`.
    pub(crate) depth_below_limit_warned: Arc<std::sync::atomic::AtomicBool>,
    /// Fires `tracing::warn!` once per instance when `search_prompt_template` has no `{query}`.
    pub(crate) missing_placeholder_warned: Arc<std::sync::atomic::AtomicBool>,
    /// Enable query-bias correction towards the user profile centroid (MM-F3, #3341).
    pub(crate) query_bias_correction: bool,
    /// Blend weight for query-bias correction (MM-F3, #3341). Clamped to `[0.0, 1.0]`.
    pub(crate) query_bias_profile_weight: f32,
    /// Cached profile centroid computed from persona-fact embeddings (MM-F3, #3341).
    ///
    /// Protected by `RwLock` to allow concurrent reads. Never holds the lock across `.await`
    /// (await-discipline rule #4). TTL-bounded; miss is non-sticky.
    pub(crate) profile_centroid: RwLock<Option<CachedCentroid>>,
    /// Time-to-live for the profile centroid cache in seconds (MM-F3, #3341). Default: 300.
    pub(crate) profile_centroid_ttl_secs: u64,
    /// Opt-in master switch for Hebbian edge-weight reinforcement (HL-F2, #3344).
    pub(crate) hebbian_enabled: bool,
    /// Weight increment applied per recall traversal when `hebbian_enabled = true` (HL-F2, #3344).
    pub(crate) hebbian_lr: f32,
    /// HL-F5 spreading activation runtime config (#3346).
    pub(crate) hebbian_spread: HelaSpreadRuntime,
}

impl SemanticMemory {
    /// Create a new `SemanticMemory` instance with default hybrid search weights (0.7/0.3).
    ///
    /// Qdrant connection is best-effort: if unavailable, semantic search is disabled.
    ///
    /// For `AppBuilder` bootstrap, prefer [`SemanticMemory::with_qdrant_ops`] to share
    /// a single gRPC channel across all subsystems.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn new(
        sqlite_path: &str,
        qdrant_url: &str,
        provider: AnyProvider,
        embedding_model: &str,
    ) -> Result<Self, MemoryError> {
        Self::with_weights(sqlite_path, qdrant_url, provider, embedding_model, 0.7, 0.3).await
    }

    /// Create a new `SemanticMemory` with custom vector/keyword weights for hybrid search.
    ///
    /// For `AppBuilder` bootstrap, prefer [`SemanticMemory::with_qdrant_ops`] to share
    /// a single gRPC channel across all subsystems.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn with_weights(
        sqlite_path: &str,
        qdrant_url: &str,
        provider: AnyProvider,
        embedding_model: &str,
        vector_weight: f64,
        keyword_weight: f64,
    ) -> Result<Self, MemoryError> {
        Self::with_weights_and_pool_size(
            sqlite_path,
            qdrant_url,
            provider,
            embedding_model,
            vector_weight,
            keyword_weight,
            5,
        )
        .await
    }

    /// Create a new `SemanticMemory` with custom weights and configurable pool size.
    ///
    /// For `AppBuilder` bootstrap, prefer [`SemanticMemory::with_qdrant_ops`] to share
    /// a single gRPC channel across all subsystems.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn with_weights_and_pool_size(
        sqlite_path: &str,
        qdrant_url: &str,
        provider: AnyProvider,
        embedding_model: &str,
        vector_weight: f64,
        keyword_weight: f64,
        pool_size: u32,
    ) -> Result<Self, MemoryError> {
        let sqlite = SqliteStore::with_pool_size(sqlite_path, pool_size).await?;
        let pool = sqlite.pool().clone();

        let qdrant = match EmbeddingStore::new(qdrant_url, pool) {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                tracing::warn!("Qdrant unavailable, semantic search disabled: {e:#}");
                None
            }
        };

        Ok(Self {
            sqlite,
            qdrant,
            provider,
            embed_provider: None,
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            importance_enabled: false,
            importance_weight: 0.15,
            tier_boost_semantic: 1.3,
            token_counter: Arc::new(TokenCounter::new()),
            graph_store: None,
            experience: None,
            reasoning: None,
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            last_qdrant_warn: Arc::new(AtomicU64::new(0)),
            admission_control: None,
            quality_gate: None,
            key_facts_dedup_threshold: 0.95,
            embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
            retrieval_depth: 0,
            search_prompt_template: String::new(),
            depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            query_bias_correction: true,
            query_bias_profile_weight: 0.25,
            profile_centroid: RwLock::new(None),
            profile_centroid_ttl_secs: 300,
            hebbian_enabled: false,
            hebbian_lr: 0.1,
            hebbian_spread: HelaSpreadRuntime::default(),
        })
    }

    /// Create a `SemanticMemory` from a pre-built `QdrantOps` instance.
    ///
    /// Use this at bootstrap to share one `QdrantOps` (and thus one gRPC channel)
    /// across all subsystems. The `ops` is consumed and wrapped inside `EmbeddingStore`.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn with_qdrant_ops(
        sqlite_path: &str,
        ops: crate::QdrantOps,
        provider: AnyProvider,
        embedding_model: &str,
        vector_weight: f64,
        keyword_weight: f64,
        pool_size: u32,
    ) -> Result<Self, MemoryError> {
        let sqlite = SqliteStore::with_pool_size(sqlite_path, pool_size).await?;
        let pool = sqlite.pool().clone();
        let store = EmbeddingStore::with_store(Box::new(ops), pool);

        Ok(Self {
            sqlite,
            qdrant: Some(Arc::new(store)),
            provider,
            embed_provider: None,
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            importance_enabled: false,
            importance_weight: 0.15,
            tier_boost_semantic: 1.3,
            token_counter: Arc::new(TokenCounter::new()),
            graph_store: None,
            experience: None,
            reasoning: None,
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            last_qdrant_warn: Arc::new(AtomicU64::new(0)),
            admission_control: None,
            quality_gate: None,
            key_facts_dedup_threshold: 0.95,
            embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
            retrieval_depth: 0,
            search_prompt_template: String::new(),
            depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            query_bias_correction: true,
            query_bias_profile_weight: 0.25,
            profile_centroid: RwLock::new(None),
            profile_centroid_ttl_secs: 300,
            hebbian_enabled: false,
            hebbian_lr: 0.1,
            hebbian_spread: HelaSpreadRuntime::default(),
        })
    }

    /// Attach a `GraphStore` for graph-aware retrieval.
    ///
    /// When set, `recall_graph` traverses the graph starting from entities
    /// matched by the query.
    #[must_use]
    pub fn with_graph_store(mut self, store: Arc<crate::graph::GraphStore>) -> Self {
        self.graph_store = Some(store);
        self
    }

    /// Attach an [`ExperienceStore`](crate::graph::experience::ExperienceStore) for tool-outcome
    /// telemetry and per-turn evolution sweeps.
    ///
    /// When set, the agent records one row per tool invocation in `experience_nodes` and
    /// periodically runs `evolution_sweep` to prune low-confidence and self-loop edges.
    #[must_use]
    pub fn with_experience_store(
        mut self,
        store: Arc<crate::graph::experience::ExperienceStore>,
    ) -> Self {
        self.experience = Some(store);
        self
    }

    /// Attach a [`ReasoningMemory`](crate::reasoning::ReasoningMemory) store for
    /// distilled reasoning strategy storage and retrieval (#3342).
    ///
    /// When set, [`SemanticMemory::retrieve_reasoning_strategies`] uses this store for
    /// embedding-similarity lookups. When `None`, retrieval returns an empty vec.
    #[must_use]
    pub fn with_reasoning(mut self, store: Arc<crate::reasoning::ReasoningMemory>) -> Self {
        self.reasoning = Some(store);
        self
    }

    /// Returns the cumulative count of community detection failures since startup.
    #[must_use]
    pub fn community_detection_failures(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.community_detection_failures.load(Ordering::Relaxed)
    }

    /// Returns the cumulative count of successful graph extractions since startup.
    #[must_use]
    pub fn graph_extraction_count(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.graph_extraction_count.load(Ordering::Relaxed)
    }

    /// Returns the cumulative count of failed graph extractions since startup.
    #[must_use]
    pub fn graph_extraction_failures(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.graph_extraction_failures.load(Ordering::Relaxed)
    }

    /// Configure temporal decay and MMR re-ranking options.
    #[must_use]
    pub fn with_ranking_options(
        mut self,
        temporal_decay_enabled: bool,
        temporal_decay_half_life_days: u32,
        mmr_enabled: bool,
        mmr_lambda: f32,
    ) -> Self {
        self.temporal_decay_enabled = temporal_decay_enabled;
        self.temporal_decay_half_life_days = temporal_decay_half_life_days;
        self.mmr_enabled = mmr_enabled;
        self.mmr_lambda = mmr_lambda;
        self
    }

    /// Configure write-time importance scoring for memory retrieval.
    #[must_use]
    pub fn with_importance_options(mut self, enabled: bool, weight: f64) -> Self {
        self.importance_enabled = enabled;
        self.importance_weight = weight;
        self
    }

    /// Configure the multiplicative score boost applied to semantic-tier messages during recall.
    ///
    /// Set to `1.0` to disable the boost. Default: `1.3`.
    #[must_use]
    pub fn with_tier_boost(mut self, boost: f64) -> Self {
        self.tier_boost_semantic = boost;
        self
    }

    /// Attach an A-MAC admission controller.
    ///
    /// When set, `remember()` and `remember_with_parts()` evaluate each message before persisting.
    /// Messages below the admission threshold return `Ok(None)` without incrementing counts.
    #[must_use]
    pub fn with_admission_control(mut self, control: AdmissionControl) -> Self {
        self.admission_control = Some(Arc::new(control));
        self
    }

    /// Attach a write quality gate that scores each `remember()` call before persisting.
    ///
    /// When set, the gate is evaluated after A-MAC admission. A `Some(reason)` result from
    /// [`crate::quality_gate::QualityGate::evaluate`] causes the write to be skipped
    /// and `Ok(None)` / `Ok((None, false))` to be returned.
    #[must_use]
    pub fn with_quality_gate(mut self, gate: Arc<crate::quality_gate::QualityGate>) -> Self {
        self.quality_gate = Some(gate);
        self
    }

    /// Set the cosine similarity threshold used to skip near-duplicate key facts on insert.
    ///
    /// When a candidate fact's nearest neighbour in `zeph_key_facts` has a score ≥ this value,
    /// the fact is not stored.  Default: `0.95`.
    #[must_use]
    pub fn with_key_facts_dedup_threshold(mut self, threshold: f32) -> Self {
        self.key_facts_dedup_threshold = threshold;
        self
    }

    /// Configure query-bias correction (MM-F3, #3341).
    ///
    /// When `enabled` is `true`, first-person queries are biased towards the user profile centroid.
    /// `profile_weight` controls the blend strength and is clamped to `[0.0, 1.0]`.
    /// `centroid_ttl_secs` controls how long the centroid cache stays valid.
    #[must_use]
    pub fn with_query_bias(
        mut self,
        enabled: bool,
        profile_weight: f32,
        centroid_ttl_secs: u64,
    ) -> Self {
        self.query_bias_correction = enabled;
        self.query_bias_profile_weight = profile_weight.clamp(0.0, 1.0);
        self.profile_centroid_ttl_secs = centroid_ttl_secs;
        self
    }

    /// Configure HL-F5 spreading activation runtime parameters (HL-F5, #3346).
    ///
    /// Has no effect when `hebbian_spread.enabled = false` (the default).
    /// Call this after `with_graph_store` and `with_hebbian` during bootstrap.
    #[must_use]
    pub fn with_hebbian_spread(mut self, runtime: HelaSpreadRuntime) -> Self {
        self.hebbian_spread = runtime;
        self
    }

    /// Configure Hebbian edge-weight reinforcement (HL-F2, #3344).
    ///
    /// When `enabled` is `true`, `lr` is added to the `weight` column of each traversed
    /// edge after every recall. `lr = 0.0` with `enabled = true` logs a WARN.
    #[must_use]
    pub fn with_hebbian(mut self, enabled: bool, lr: f32) -> Self {
        let lr = lr.max(0.0);
        if enabled && lr == 0.0 {
            tracing::warn!("hebbian enabled with lr=0.0 — no reinforcement will occur");
        }
        self.hebbian_enabled = enabled;
        self.hebbian_lr = lr;
        self
    }

    /// Classify a query's intent for query-bias correction (MM-F3, #3341).
    ///
    /// Returns [`QueryIntent::FirstPerson`] when the query contains self-referential language
    /// (first-person pronouns). Otherwise returns [`QueryIntent::Other`].
    pub(crate) fn classify_query_intent(query: &str) -> QueryIntent {
        if persona::contains_self_referential_language(query) {
            QueryIntent::FirstPerson
        } else {
            QueryIntent::Other
        }
    }

    /// Apply query-bias correction to an embedding (MM-F3, #3341).
    ///
    /// Returns the embedding unchanged if `query_bias_correction` is `false`,
    /// if the query is not first-person, or if the profile centroid is unavailable.
    /// Logs a single WARN on dimension mismatch and returns the original embedding.
    #[tracing::instrument(name = "memory.query_bias.apply", skip(self, embedding), fields(query_len = query.len()))]
    pub(crate) async fn apply_query_bias(&self, query: &str, embedding: Vec<f32>) -> Vec<f32> {
        if !self.query_bias_correction {
            tracing::debug!(reason = "disabled", "query-bias: skipping");
            return embedding;
        }
        if Self::classify_query_intent(query) != QueryIntent::FirstPerson {
            tracing::debug!(reason = "not_first_person", "query-bias: skipping");
            return embedding;
        }
        let Some(centroid) = self.profile_centroid_cached().await else {
            tracing::debug!(reason = "no_centroid", "query-bias: skipping");
            return embedding;
        };
        if centroid.len() != embedding.len() {
            tracing::warn!(
                centroid_dim = centroid.len(),
                query_dim = embedding.len(),
                reason = "dim_mismatch",
                "query-bias: dimension mismatch between profile centroid and query embedding — skipping bias"
            );
            return embedding;
        }
        let w = self.query_bias_profile_weight;
        tracing::debug!(
            intent = "first_person",
            centroid_dim = centroid.len(),
            weight = w,
            "query-bias: applying profile bias"
        );
        embedding
            .iter()
            .zip(centroid.iter())
            .map(|(&q, &c)| (1.0 - w) * q + w * c)
            .collect()
    }

    /// Return the cached profile centroid, recomputing if stale or absent (MM-F3, #3341).
    ///
    /// Holds the read lock only to check freshness; releases it before any `.await`.
    /// On compute failure, preserves the previous cache value (non-sticky miss).
    #[tracing::instrument(name = "memory.query_bias.centroid", skip(self))]
    pub(crate) async fn profile_centroid_cached(&self) -> Option<Vec<f32>> {
        // Fast path: check freshness under read lock without holding it across await.
        {
            let guard = self.profile_centroid.read().await;
            if let Some(c) = &*guard
                && c.computed_at.elapsed().as_secs() < self.profile_centroid_ttl_secs
            {
                let ttl_remaining = self
                    .profile_centroid_ttl_secs
                    .saturating_sub(c.computed_at.elapsed().as_secs());
                tracing::debug!(
                    centroid_dim = c.vector.len(),
                    ttl_remaining_secs = ttl_remaining,
                    "query-bias: centroid cache hit"
                );
                return Some(c.vector.clone());
            }
        }
        // Slow path: recompute. Guard is dropped before this point.
        let computed = self.compute_profile_centroid().await;
        let mut guard = self.profile_centroid.write().await;
        match computed {
            Some(v) => {
                tracing::debug!(centroid_dim = v.len(), "query-bias: centroid computed");
                *guard = Some(CachedCentroid {
                    vector: v.clone(),
                    computed_at: Instant::now(),
                });
                Some(v)
            }
            None => {
                // Do not overwrite a valid (but stale) cache on failure — serve stale over nothing.
                guard.as_ref().map(|c| c.vector.clone())
            }
        }
    }

    /// Compute the profile centroid from persona-fact embeddings (MM-F3, #3341).
    ///
    /// Returns `None` when the persona table is empty or embedding fails.
    /// Uses `load_persona_facts(0.0)` (all non-superseded facts) for the centroid basis.
    async fn compute_profile_centroid(&self) -> Option<Vec<f32>> {
        let facts = match self.sqlite.load_persona_facts(0.0).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "query-bias: failed to load persona facts");
                return None;
            }
        };
        if facts.is_empty() {
            return None;
        }
        let provider = self.effective_embed_provider();
        let texts: Vec<String> = facts.iter().map(|f| f.content.clone()).collect();
        let mut embeddings: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for text in &texts {
            match provider.embed(text).await {
                Ok(v) => embeddings.push(v),
                Err(e) => {
                    tracing::warn!(error = %e, "query-bias: failed to embed persona fact — skipping");
                }
            }
        }
        if embeddings.is_empty() {
            return None;
        }
        let dim = embeddings[0].len();
        let mut centroid = vec![0.0f32; dim];
        for emb in &embeddings {
            if emb.len() != dim {
                tracing::warn!(
                    expected = dim,
                    got = emb.len(),
                    "query-bias: persona embedding dimension mismatch — skipping fact"
                );
                continue;
            }
            for (c, &v) in centroid.iter_mut().zip(emb.iter()) {
                *c += v;
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let n = embeddings.len() as f32;
        for c in &mut centroid {
            *c /= n;
        }
        Some(centroid)
    }

    /// Configure retrieval depth and search prompt template (MM-F1/F2, #3340).
    ///
    /// `depth` is the number of ANN candidates fetched from the vector store before keyword merge
    /// and MMR re-ranking.  `0` = legacy behavior (`recall_limit * 2`).  `≥ 1` = exact count.
    ///
    /// `search_prompt_template` is applied to the raw user query before embedding.  Supports a
    /// single `{query}` placeholder.  Empty string = identity.
    #[must_use]
    pub fn with_retrieval_options(
        mut self,
        depth: u32,
        search_prompt_template: impl Into<String>,
    ) -> Self {
        self.retrieval_depth = depth;
        self.search_prompt_template = search_prompt_template.into();
        self
    }

    /// Effective ANN candidate count for a given requested final limit (MM-F1, #3340).
    ///
    /// - `retrieval_depth == 0`: legacy behavior, returns `limit * 2`.
    /// - `retrieval_depth >= 1`: returns the configured depth directly.
    ///
    /// When `retrieval_depth < limit`, a one-shot WARN fires because the ANN pool cannot
    /// saturate the requested top-k.  When `limit <= retrieval_depth < limit * 2`, an INFO
    /// fires per call noting the smaller-than-legacy pool.
    pub(crate) fn effective_depth(&self, limit: usize) -> usize {
        use std::sync::atomic::Ordering;

        let depth = self.retrieval_depth as usize;
        if depth == 0 {
            return limit.saturating_mul(2);
        }
        if depth < limit {
            if !self.depth_below_limit_warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    retrieval_depth = depth,
                    recall_limit = limit,
                    "memory.retrieval.depth < recall_limit; ANN pool cannot saturate top-k — consider raising depth"
                );
            }
        } else if depth < limit.saturating_mul(2) {
            tracing::info!(
                retrieval_depth = depth,
                recall_limit = limit,
                legacy_default = limit.saturating_mul(2),
                "memory.retrieval.depth is below legacy limit*2; ANN pool will be smaller than pre-#3340"
            );
        } else {
            tracing::debug!(
                retrieval_depth = depth,
                recall_limit = limit,
                "recall: using configured ANN depth"
            );
        }
        depth
    }

    /// Apply the configured search prompt template to a raw query (MM-F2, #3340).
    ///
    /// Returns `query` as-is when the template is empty or has no `{query}` placeholder.
    /// A one-shot WARN fires when the template is non-empty but missing the placeholder.
    pub(crate) fn apply_search_prompt(&self, query: &str) -> String {
        use std::sync::atomic::Ordering;

        let template = &self.search_prompt_template;
        if template.is_empty() {
            return query.to_owned();
        }
        if !template.contains("{query}") {
            if !self
                .missing_placeholder_warned
                .swap(true, Ordering::Relaxed)
            {
                tracing::warn!(
                    template = template.as_str(),
                    "memory.retrieval.search_prompt_template has no {{query}} placeholder — \
                     using raw query as-is"
                );
            }
            return query.to_owned();
        }
        template.replace("{query}", query)
    }

    /// Attach a dedicated embedding provider for write-path and backfill operations.
    ///
    /// When set, all batch embedding calls (backfill, `remember`) route through this provider
    /// instead of the main `provider`. This prevents `embed_backfill` from saturating the main
    /// provider and causing guardrail timeouts due to rate-limit contention or Ollama model-lock.
    #[must_use]
    pub fn with_embed_provider(mut self, embed_provider: AnyProvider) -> Self {
        self.embed_provider = Some(embed_provider);
        self
    }

    /// Returns the provider to use for embedding calls.
    ///
    /// Returns the dedicated embed provider when configured, falling back to the main provider.
    pub fn effective_embed_provider(&self) -> &AnyProvider {
        self.embed_provider.as_ref().unwrap_or(&self.provider)
    }

    /// Construct a `SemanticMemory` from pre-built parts.
    ///
    /// Intended for tests that need full control over the backing stores.
    #[must_use]
    pub fn from_parts(
        sqlite: SqliteStore,
        qdrant: Option<Arc<EmbeddingStore>>,
        provider: AnyProvider,
        embedding_model: impl Into<String>,
        vector_weight: f64,
        keyword_weight: f64,
        token_counter: Arc<TokenCounter>,
    ) -> Self {
        Self {
            sqlite,
            qdrant,
            provider,
            embed_provider: None,
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            importance_enabled: false,
            importance_weight: 0.15,
            tier_boost_semantic: 1.3,
            token_counter,
            graph_store: None,
            experience: None,
            reasoning: None,
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            last_qdrant_warn: Arc::new(AtomicU64::new(0)),
            admission_control: None,
            quality_gate: None,
            key_facts_dedup_threshold: 0.95,
            embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
            retrieval_depth: 0,
            search_prompt_template: String::new(),
            depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            query_bias_correction: true,
            query_bias_profile_weight: 0.25,
            profile_centroid: RwLock::new(None),
            profile_centroid_ttl_secs: 300,
            hebbian_enabled: false,
            hebbian_lr: 0.1,
            hebbian_spread: HelaSpreadRuntime::default(),
        }
    }

    /// Create a `SemanticMemory` using the `SQLite`-embedded vector backend.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn with_sqlite_backend(
        sqlite_path: &str,
        provider: AnyProvider,
        embedding_model: &str,
        vector_weight: f64,
        keyword_weight: f64,
    ) -> Result<Self, MemoryError> {
        Self::with_sqlite_backend_and_pool_size(
            sqlite_path,
            provider,
            embedding_model,
            vector_weight,
            keyword_weight,
            5,
        )
        .await
    }

    /// Create a `SemanticMemory` using the `SQLite`-embedded vector backend with configurable pool size.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn with_sqlite_backend_and_pool_size(
        sqlite_path: &str,
        provider: AnyProvider,
        embedding_model: &str,
        vector_weight: f64,
        keyword_weight: f64,
        pool_size: u32,
    ) -> Result<Self, MemoryError> {
        let sqlite = SqliteStore::with_pool_size(sqlite_path, pool_size).await?;
        let pool = sqlite.pool().clone();
        let store = EmbeddingStore::new_sqlite(pool);

        Ok(Self {
            sqlite,
            qdrant: Some(Arc::new(store)),
            provider,
            embed_provider: None,
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            importance_enabled: false,
            importance_weight: 0.15,
            tier_boost_semantic: 1.3,
            token_counter: Arc::new(TokenCounter::new()),
            graph_store: None,
            experience: None,
            reasoning: None,
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            last_qdrant_warn: Arc::new(AtomicU64::new(0)),
            admission_control: None,
            quality_gate: None,
            key_facts_dedup_threshold: 0.95,
            embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
            retrieval_depth: 0,
            search_prompt_template: String::new(),
            depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            query_bias_correction: true,
            query_bias_profile_weight: 0.25,
            profile_centroid: RwLock::new(None),
            profile_centroid_ttl_secs: 300,
            hebbian_enabled: false,
            hebbian_lr: 0.1,
            hebbian_spread: HelaSpreadRuntime::default(),
        })
    }

    /// Access the underlying `SqliteStore` for operations that don't involve semantics.
    #[must_use]
    pub fn sqlite(&self) -> &SqliteStore {
        &self.sqlite
    }

    /// Check if the vector store backend is reachable.
    ///
    /// Performs a real health check (Qdrant gRPC ping or `SQLite` query)
    /// instead of just checking whether the client was created.
    pub async fn is_vector_store_connected(&self) -> bool {
        match self.qdrant.as_ref() {
            Some(store) => store.health_check().await,
            None => false,
        }
    }

    /// Check if a vector store client is configured (may not be connected).
    #[must_use]
    pub fn has_vector_store(&self) -> bool {
        self.qdrant.is_some()
    }

    /// Return a reference to the embedding store, if configured.
    #[must_use]
    pub fn embedding_store(&self) -> Option<&Arc<EmbeddingStore>> {
        self.qdrant.as_ref()
    }

    /// Return a reference to the underlying LLM provider (used for RPE embedding).
    pub fn provider(&self) -> &AnyProvider {
        &self.provider
    }

    /// Count messages in a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn message_count(
        &self,
        conversation_id: crate::types::ConversationId,
    ) -> Result<i64, MemoryError> {
        self.sqlite.count_messages(conversation_id).await
    }

    /// Count messages not yet covered by any summary.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn unsummarized_message_count(
        &self,
        conversation_id: crate::types::ConversationId,
    ) -> Result<i64, MemoryError> {
        let after_id = self
            .sqlite
            .latest_summary_last_message_id(conversation_id)
            .await?
            .unwrap_or(crate::types::MessageId(0));
        self.sqlite
            .count_messages_after(conversation_id, after_id)
            .await
    }

    /// Load recent episodic messages for the promotion-scan window.
    ///
    /// Returns up to `max_items` of the most recent non-deleted messages across all
    /// conversations, with their `conversation_id` for session-count heuristics.
    ///
    /// # Embedding note
    ///
    /// `embedding` is returned as `None` in this MVP implementation. A future pass
    /// will join with the Qdrant payload to populate embeddings inline.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if the underlying `SQLite` query fails.
    // TODO(review): populate embeddings by fetching from Qdrant when available.
    pub async fn load_promotion_window(
        &self,
        max_items: usize,
    ) -> Result<Vec<crate::compression::promotion::PromotionInput>, MemoryError> {
        use zeph_db::sql;

        let limit = i64::try_from(max_items).unwrap_or(i64::MAX);
        let rows: Vec<(
            crate::types::MessageId,
            crate::types::ConversationId,
            String,
        )> = zeph_db::query_as(sql!(
            "SELECT id, conversation_id, content \
                 FROM messages \
                 WHERE deleted_at IS NULL \
                 ORDER BY id DESC \
                 LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(self.sqlite.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|(message_id, conversation_id, content)| {
                crate::compression::promotion::PromotionInput {
                    message_id,
                    conversation_id,
                    content,
                    // Embeddings not wired yet — scan will skip rows with None.
                    embedding: None,
                }
            })
            .collect())
    }

    /// Retrieve top-k reasoning strategies by embedding similarity to `query`.
    ///
    /// Returns an empty vec when reasoning memory is not attached, Qdrant is unavailable,
    /// or the provider does not support embeddings.
    ///
    /// This method is **pure** — it does not increment `use_count` or `last_used_at`.
    /// Call [`crate::reasoning::ReasoningMemory::mark_used`] with the ids of strategies
    /// actually injected into the prompt (after budget truncation).
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation or the vector search fails.
    pub async fn retrieve_reasoning_strategies(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::reasoning::ReasoningStrategy>, MemoryError> {
        let Some(reasoning) = &self.reasoning else {
            return Ok(Vec::new());
        };
        if !self.effective_embed_provider().supports_embeddings() {
            return Ok(Vec::new());
        }
        let embedding = self.effective_embed_provider().embed(query).await?;
        reasoning
            .retrieve_by_embedding(&embedding, limit as u64)
            .await
    }
}
