// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SQLite-backed conversation persistence with Qdrant vector search.

pub mod anchored_summary;
pub mod compaction_probe;
pub mod compression_guidelines;
pub mod document;

pub mod embedding_registry;
pub mod embedding_store;
pub mod error;
pub mod eviction;
pub mod graph;
pub mod in_memory_store;
pub mod math;
pub mod qdrant_ops;
pub mod response_cache;
pub mod router;
pub mod semantic;
pub mod snapshot;
pub mod sqlite;
pub mod sqlite_vector_store;
pub mod testing;
pub mod token_counter;
pub mod types;
pub mod vector_store;

pub use anchored_summary::AnchoredSummary;
pub use compaction_probe::{
    CompactionProbeConfig, CompactionProbeResult, ProbeQuestion, ProbeVerdict,
    answer_probe_questions, generate_probe_questions, score_answers, validate_compaction,
};
pub use compression_guidelines::CompressionGuidelinesConfig;
#[cfg(feature = "compression-guidelines")]
pub use compression_guidelines::{
    build_guidelines_update_prompt, sanitize_guidelines, start_guidelines_updater,
    truncate_to_token_budget, update_guidelines_once,
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
pub use graph::{Community, Edge, EdgeType, Entity, EntityType, GraphFact, GraphStore};
pub use math::cosine_similarity;
pub use qdrant_ops::QdrantOps;
pub use response_cache::ResponseCache;
pub use router::{
    HeuristicRouter, MemoryRoute, MemoryRouter, TemporalRange, classify_graph_subgraph,
    strip_temporal_keywords,
};
pub use semantic::{
    ExtractionResult, ExtractionStats, GraphExtractionConfig, LinkingStats, NoteLinkingConfig,
    StructuredSummary, build_summarization_prompt, extract_and_store, link_memory_notes,
};
pub use snapshot::{ImportStats, MemorySnapshot, export_snapshot, import_snapshot};
#[cfg(feature = "compression-guidelines")]
pub use sqlite::compression_guidelines::CompressionFailurePair;
pub use sqlite::corrections::UserCorrectionRow;
#[cfg(feature = "experiments")]
pub use sqlite::experiments::{ExperimentResultRow, NewExperimentResult, SessionSummaryRow};
pub use token_counter::TokenCounter;
pub use tokio_util::sync::CancellationToken;
pub use types::{ConversationId, MessageId};
pub use vector_store::{
    FieldCondition, FieldValue, ScoredVectorPoint, VectorFilter, VectorPoint, VectorStore,
    VectorStoreError,
};
