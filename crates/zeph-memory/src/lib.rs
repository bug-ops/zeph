//! SQLite-backed conversation persistence with Qdrant vector search.

pub mod embedding_store;
pub mod error;
#[cfg(feature = "mock")]
pub mod in_memory_store;
pub mod qdrant_ops;
pub mod semantic;
pub mod sqlite;
pub mod types;
pub mod vector_store;

pub use embedding_store::ensure_qdrant_collection;
pub use error::MemoryError;
pub use qdrant_ops::QdrantOps;
pub use semantic::estimate_tokens;
pub use types::{ConversationId, MessageId};
pub use vector_store::{
    FieldCondition, FieldValue, ScoredVectorPoint, VectorFilter, VectorPoint, VectorStore,
    VectorStoreError,
};
