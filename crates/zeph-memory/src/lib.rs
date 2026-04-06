// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Database-backed conversation persistence with Qdrant vector search.

pub mod admission;
pub mod admission_rl;
pub mod anchored_summary;
pub mod compaction_probe;
pub mod compression_guidelines;
pub mod compression_predictor;
pub mod consolidation;
pub mod document;
pub mod forgetting;
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
pub mod response_cache;
pub mod router;
pub mod semantic;
pub mod snapshot;
pub mod store;
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
    CategoryScore, CompactionProbeConfig, CompactionProbeResult, ProbeCategory, ProbeQuestion,
    ProbeVerdict, answer_probe_questions, generate_probe_questions, score_answers,
    validate_compaction,
};
pub use compression_guidelines::CompressionGuidelinesConfig;
pub use compression_guidelines::{
    build_guidelines_update_prompt, sanitize_guidelines, start_guidelines_updater,
    truncate_to_token_budget, update_guidelines_once,
};
pub use compression_predictor::{
    CompressionFeatures, CompressionModelWeights, CompressionPredictor,
};
pub use consolidation::{
    ConsolidationConfig, ConsolidationResult, TopologyOp, start_consolidation_loop,
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
pub use eviction::{EbbinghausPolicy, EvictionConfig, EvictionPolicy, start_eviction_loop};
pub use forgetting::{ForgettingConfig, ForgettingResult, start_forgetting_loop};
pub use graph::EntityLockManager;
pub use graph::{
    BeliefRevisionConfig, Community, Edge, EdgeType, Entity, EntityType, GraphFact, GraphStore,
    RpeRouter, RpeSignal, extract_candidate_entities,
};
pub use qdrant_ops::QdrantOps;
pub use response_cache::ResponseCache;
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
    LinkingStats, NoteLinkingConfig, PersonaExtractionConfig, StructuredSummary, TrajectoryEntry,
    TrajectoryExtractionConfig, TreeConsolidationConfig, TreeConsolidationResult, WriteBuffer,
    build_summarization_prompt, contains_self_referential_language, extract_and_store,
    extract_persona_facts, extract_trajectory_entries, link_memory_notes,
    run_tree_consolidation_sweep, start_tree_consolidation_loop,
};
pub use snapshot::{ImportStats, MemorySnapshot, export_snapshot, import_snapshot};
pub use store::compression_guidelines::CompressionFailurePair;
pub use store::corrections::UserCorrectionRow;
pub use store::experiments::{ExperimentResultRow, NewExperimentResult, SessionSummaryRow};
pub use store::memory_tree::MemoryTreeRow;
pub use store::persona::PersonaFactRow;
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
