// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SQLite-backed conversation persistence with Qdrant vector search.

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
pub use graph::{Community, Edge, Entity, EntityType, GraphFact, GraphStore};
pub use math::cosine_similarity;
pub use qdrant_ops::QdrantOps;
pub use response_cache::ResponseCache;
pub use router::{HeuristicRouter, MemoryRoute, MemoryRouter};
pub use semantic::{ExtractionStats, GraphExtractionConfig, extract_and_store};
pub use snapshot::{ImportStats, MemorySnapshot, export_snapshot, import_snapshot};
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
