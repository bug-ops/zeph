//! AST-based code indexing, semantic retrieval, and repo map generation.
//!
//! Provides a Code RAG pipeline: tree-sitter parses source into AST chunks,
//! chunks are embedded and stored in Qdrant, and retrieved via hybrid search
//! (semantic + grep routing) for injection into the agent context window.

pub mod chunker;
pub mod context;
pub mod indexer;
pub mod languages;
pub mod repo_map;
pub mod retriever;
pub mod store;
pub mod watcher;
