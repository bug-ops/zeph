// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AST-based code indexing, semantic retrieval, and repo map generation for Zeph.
//!
//! # Overview
//!
//! `zeph-index` implements the **Code RAG** (Retrieval-Augmented Generation) pipeline
//! that gives the Zeph agent grounded awareness of a local codebase. The pipeline has
//! three stages:
//!
//! 1. **Chunking** — [`chunker`] uses tree-sitter to parse source files into
//!    semantically meaningful AST-level chunks (functions, structs, impl blocks, …)
//!    rather than fixed-size text windows.
//! 2. **Indexing** — [`indexer`] embeds every chunk via the configured LLM provider
//!    and writes the vector + rich metadata into a dual store: Qdrant for vector
//!    similarity and `SQLite` for exact hash deduplication.
//! 3. **Retrieval** — [`retriever`] classifies the incoming query as *semantic*,
//!    *grep*, or *hybrid*, embeds the query, searches Qdrant, applies a score
//!    threshold, and packs results within a token budget.
//!
//! # Additional subsystems
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`repo_map`] | Compact `<repo_map>` for the system prompt — file paths + symbol signatures |
//! | [`mcp_server`] | In-process MCP server exposing `symbol_definition`, `find_text_references`, `call_graph`, `module_summary` tools |
//! | [`watcher`] | File-system watcher that triggers incremental re-indexing on saves |
//! | [`languages`] | Language detection and tree-sitter grammar registry |
//! | [`store`] | Qdrant + `SQLite` dual-write store |
//! | [`error`] | Unified error type [`IndexError`] |
//!
//! # Quick start
//!
//! ```no_run
//! use std::sync::Arc;
//! use zeph_index::indexer::{CodeIndexer, IndexerConfig};
//! use zeph_index::retriever::{CodeRetriever, RetrievalConfig};
//! use zeph_index::store::CodeStore;
//! # async fn example() -> zeph_index::Result<()> {
//! # let store: CodeStore = panic!("placeholder");
//! # let provider: Arc<zeph_llm::any::AnyProvider> = panic!("placeholder");
//!
//! // Build and run initial project index.
//! let indexer = CodeIndexer::new(store.clone(), Arc::clone(&provider), IndexerConfig::default());
//! let report = indexer.index_project(std::path::Path::new("."), None).await?;
//! println!("{} chunks indexed", report.chunks_created);
//!
//! // Retrieve relevant code for a query.
//! let retriever = CodeRetriever::new(store, Arc::clone(&provider), RetrievalConfig::default());
//! let result = retriever.retrieve("how does authentication work?", 8_000).await?;
//! println!("{} chunks, {} tokens", result.chunks.len(), result.total_tokens);
//! # Ok(())
//! # }
//! ```

#[allow(unused_imports)]
pub(crate) use zeph_db::sql;

pub mod chunker;
pub mod context;
pub mod error;
pub mod indexer;
pub mod languages;
pub mod mcp_server;
pub mod repo_map;
pub mod retriever;
pub mod store;
pub mod watcher;

pub use error::{IndexError, Result};
pub use indexer::IndexProgress;
pub use mcp_server::IndexMcpServer;
