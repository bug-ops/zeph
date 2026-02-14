//! AST-based code indexing, semantic retrieval, and repo map generation.
//!
//! Provides a Code RAG pipeline: tree-sitter parses source into AST chunks,
//! chunks are embedded and stored in Qdrant, and retrieved via hybrid search
//! (semantic + grep routing) for injection into the agent context window.

pub(crate) mod chunker;
pub(crate) mod context;
pub mod error;
pub mod indexer;
pub(crate) mod languages;
pub mod repo_map;
pub mod retriever;
pub mod store;
pub mod watcher;

pub use error::{IndexError, Result};
