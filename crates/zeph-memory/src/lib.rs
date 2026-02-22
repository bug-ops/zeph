//! SQLite-backed conversation persistence with Qdrant vector search.

pub mod document;
pub mod embedding_registry;
pub mod embedding_store;
pub mod error;
#[cfg(feature = "mock")]
pub mod in_memory_store;
pub mod qdrant_ops;
pub mod response_cache;
pub mod semantic;
pub mod snapshot;
pub mod sqlite;
pub mod sqlite_vector_store;
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
pub use qdrant_ops::QdrantOps;
pub use response_cache::ResponseCache;
pub use semantic::estimate_tokens;
pub use snapshot::{ImportStats, MemorySnapshot, export_snapshot, import_snapshot};
pub use types::{ConversationId, MessageId};
pub use vector_store::{
    FieldCondition, FieldValue, ScoredVectorPoint, VectorFilter, VectorPoint, VectorStore,
    VectorStoreError,
};
