// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Semantic memory layer for the Zeph agent.
//!
//! `zeph-memory` implements a two-backend hybrid memory system:
//!
//! - **[`store::DbStore`]** (`SqliteStore`) — relational persistence for messages, summaries,
//!   persona facts, trajectory entries, and session metadata.
//! - **[`embedding_store::EmbeddingStore`]** — Qdrant-backed vector index for semantic recall.
//!   Falls back gracefully to [`db_vector_store::DbVectorStore`] when Qdrant is unavailable.
//!
//! The high-level entry point is [`semantic::SemanticMemory`], which combines both backends
//! and exposes `remember` / `recall` / `summarize` operations consumed by `zeph-core`.
//!
//! # Architecture overview
//!
//! ```text
//! SemanticMemory
//! ├── SqliteStore  ── messages, summaries, corrections, persona, trajectory …
//! └── EmbeddingStore ── Qdrant (primary) / DbVectorStore (fallback)
//!         └── QdrantOps  ── thin gRPC wrapper over qdrant-client
//! ```
//!
//! # Memory tiers
//!
//! Messages are classified into four tiers (see [`types::MemoryTier`]):
//!
//! | Tier | Description |
//! |------|-------------|
//! | `Working` | Current context window; never persisted. |
//! | `Episodic` | Per-session messages stored in `SQLite`. |
//! | `Semantic` | Cross-session distilled facts promoted from episodic. |
//! | `Persona` | Long-lived user attributes (preferences, domain knowledge). |
//!
//! # Admission control
//!
//! Each `remember()` call is gated by [`admission::AdmissionControl`] (A-MAC, #2317), which
//! evaluates five factors (future utility, factual confidence, semantic novelty, temporal
//! recency, content-type prior) and rejects low-value messages before they reach the DB.
//!
//! # Memory routing
//!
//! [`router::HybridRouter`] classifies each recall query and dispatches to the appropriate
//! backend: keyword (`SQLite` FTS5), semantic (Qdrant), graph (BFS traversal), episodic
//! (timestamp-filtered FTS5), or hybrid (reciprocal-rank fusion of keyword + semantic).
//!
//! # Background loops
//!
//! Several background tasks maintain memory health:
//!
//! - [`eviction::start_eviction_loop`] — Ebbinghaus-curve eviction.
//! - [`forgetting::start_forgetting_loop`] — `SleepGate` importance downscaling.
//! - [`consolidation::start_consolidation_loop`] — cross-session fact merging.
//! - [`tiers::start_tier_promotion_loop`] — Episodic → Semantic promotion.
//! - [`semantic::start_tree_consolidation_loop`] — hierarchical note consolidation.
//! - [`hebbian_consolidation::spawn_consolidation_loop`] — HL-F3/F4 cluster distillation.
//!
//! # Feature flags
//!
//! | Feature | Description |
//! |---------|-------------|
//! | `sqlite` (default) | Enable SQLite persistence via `zeph-db`. |
//! | `pdf` | Enable `PdfLoader` for PDF ingestion. |
//! | `postgres` | Enable PostgreSQL support via `zeph-db`. |

pub mod admission;
pub mod anchored_summary;
pub mod compaction_probe;
pub mod compression;
pub mod compression_guidelines;
pub mod consolidation;
pub mod document;
pub mod facade;
pub mod forgetting;
pub mod hebbian_consolidation;
pub mod reasoning;
pub mod recall_view;
pub mod retrieval_failure_logger;
pub mod scenes;
pub mod tiers;

pub mod db_vector_store;
pub mod embedding_registry;
pub mod embedding_store;
pub mod error;
pub mod eviction;
pub mod graph;
pub mod in_memory_store;
pub mod qdrant_ops;
pub mod quality_gate;
pub mod response_cache;
pub mod router;
pub mod semantic;
pub mod snapshot;
pub mod store;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod token_counter;
pub mod types;
pub mod vector_store;

pub use admission::{
    AdmissionControl, AdmissionDecision, AdmissionFactors, AdmissionRejected, AdmissionWeights,
    GoalGateConfig, compute_content_type_prior, compute_factual_confidence, log_admission_decision,
};
pub use anchored_summary::AnchoredSummary;
pub use compaction_probe::{
    CategoryScore, CompactionProbeResult, ProbeQuestion, ProbeVerdict, answer_probe_questions,
    generate_probe_questions, score_answers, validate_compaction,
};
pub use compression::{
    CompressionLevel, RetrievalPolicy,
    promotion::{
        PromotionCandidate, PromotionConfig, PromotionEngine, PromotionInput, SkillWriter,
    },
};
pub use compression_guidelines::{
    build_guidelines_update_prompt, sanitize_guidelines, start_guidelines_updater,
    truncate_to_token_budget, update_guidelines_once,
};
pub use consolidation::{
    ConsolidationConfig, ConsolidationResult, TopologyOp, run_consolidation_sweep,
    start_consolidation_loop,
};
#[cfg(feature = "pdf")]
pub use document::PdfLoader;
pub use document::{
    Chunk, Document, DocumentError, DocumentLoader, DocumentMetadata, IngestionPipeline,
    SplitterConfig, TextLoader, TextSplitter,
};
pub use embedding_registry::{
    EmbedFuture, Embeddable, EmbeddingRegistry, EmbeddingRegistryError, SyncStats,
};
pub use embedding_store::ensure_qdrant_collection;
pub use error::MemoryError;
pub use eviction::{EbbinghausPolicy, EvictionPolicy, start_eviction_loop};
pub use facade::{
    CompactionContext, CompactionResult, InMemoryFacade, MemoryEntry, MemoryFacade, MemoryMatch,
    MemorySource,
};
pub use forgetting::{ForgettingConfig, ForgettingResult, start_forgetting_loop};
pub use graph::EntityLockManager;
pub use graph::experience::{EvolutionSweepStats, ExperienceStore};
pub use graph::{
    BeliefRevisionConfig, Community, Edge, EdgeType, Entity, EntityType, GraphFact, GraphStore,
    RpeRouter, RpeSignal, extract_candidate_entities,
};
pub use hebbian_consolidation::{
    GraphRule, HebbianConsolidationCandidate, HebbianConsolidationOutcome,
    run_consolidation_sweep as run_hebbian_consolidation_sweep,
    spawn_consolidation_loop as spawn_hebbian_consolidation_loop,
};
pub use qdrant_ops::QdrantOps;
pub use reasoning::{
    Outcome, ProcessTurnConfig, ReasoningMemory, ReasoningStrategy, SelfJudgeOutcome,
    distill_strategy, process_turn as process_reasoning_turn, run_self_judge,
};
pub use recall_view::{RecallView, RecalledFact};
pub use response_cache::ResponseCache;
pub use retrieval_failure_logger::RetrievalFailureLogger;
pub use router::{
    AsyncMemoryRouter, HeuristicRouter, HybridRouter, LlmRouter, MemoryRoute, MemoryRouter,
    RoutingDecision, TemporalRange, classify_graph_subgraph, parse_route_str,
    strip_temporal_keywords,
};
pub use scenes::{
    MemScene, SceneConfig, consolidate_scenes, list_scenes, start_scene_consolidation_loop,
};
pub use semantic::{
    BufferedWrite, EmbedContext, ExtractionResult, ExtractionStats, GraphExtractionConfig,
    HebbianReinforcement, HelaSpreadRuntime, ImportanceScoring, LinkingStats, MmrReranking,
    NoteLinkingConfig, PersonaExtractionConfig, QueryBiasCorrection, RecalledMessage,
    StructuredSummary, TemporalDecay, TrajectoryEntry, TrajectoryExtractionConfig,
    TreeConsolidationConfig, TreeConsolidationResult, WriteBuffer, build_summarization_prompt,
    contains_self_referential_language, extract_and_store, extract_persona_facts,
    extract_trajectory_entries, link_memory_notes, run_tree_consolidation_sweep,
    start_tree_consolidation_loop,
};
pub use snapshot::{ImportStats, MemorySnapshot, export_snapshot, import_snapshot};
pub use store::compression_guidelines::CompressionFailurePair;
pub use store::corrections::UserCorrectionRow;
pub use store::experiments::{ExperimentResultRow, NewExperimentResult, SessionSummaryRow};
pub use store::memory_tree::MemoryTreeRow;
pub use store::persona::PersonaFactRow;
pub use store::retrieval_failures::{RetrievalFailureRecord, RetrievalFailureType};
pub use store::session_digest::SessionDigest;
pub use store::trajectory::{NewTrajectoryEntry, TrajectoryEntryRow};
pub use tiers::{TierPromotionConfig, start_tier_promotion_loop};
pub use token_counter::TokenCounter;
pub use tokio_util::sync::CancellationToken;
pub use types::{ConversationId, MemSceneId, MemoryTier, MessageId};
pub use vector_store::{
    FieldCondition, FieldValue, ScoredVectorPoint, VectorFilter, VectorPoint, VectorStore,
    VectorStoreError,
};
pub use zeph_common::config::memory::HebbianConsolidationConfig;
pub use zeph_config::memory::CompressionGuidelinesConfig;
pub use zeph_config::memory::EvictionConfig;
pub use zeph_config::memory::{CompactionProbeConfig, ProbeCategory};
