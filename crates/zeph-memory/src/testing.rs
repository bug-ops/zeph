// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test helpers for `zeph-memory`.
//!
//! Provides `mock_semantic_memory` — a convenience constructor that creates a
//! fully-wired [`SemanticMemory`] backed by an in-memory `SQLite` database and
//! [`InMemoryVectorStore`], so tests do not need a real Qdrant or filesystem.

use std::sync::Arc;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;

use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::in_memory_store::InMemoryVectorStore;
use crate::semantic::SemanticMemory;
use crate::store::SqliteStore;
use crate::token_counter::TokenCounter;

/// Build a [`SemanticMemory`] that runs entirely in-process with no external
/// dependencies.
///
/// - `SQLite` backend: `:memory:` (in-process, no file I/O)
/// - Vector backend: [`InMemoryVectorStore`] (cosine similarity search)
/// - LLM provider: [`MockProvider`] with embedding support enabled
///
/// # Errors
///
/// Returns `MemoryError` if the in-memory `SQLite` cannot be initialised.
pub async fn mock_semantic_memory() -> Result<Arc<SemanticMemory>, MemoryError> {
    let mut mock = MockProvider::default();
    mock.supports_embeddings = true;
    mock.embedding = vec![0.1_f32; 384];
    let provider = AnyProvider::Mock(mock);

    // `:memory:` creates an in-process SQLite database — no disk I/O.
    let sqlite = SqliteStore::with_pool_size(":memory:", 1).await?;
    let pool = sqlite.pool().clone();

    // InMemoryVectorStore satisfies the VectorStore trait without Qdrant.
    let qdrant = Some(Arc::new(EmbeddingStore::with_store(
        Box::new(InMemoryVectorStore::new()),
        pool,
    )));

    Ok(Arc::new(SemanticMemory::from_parts(
        sqlite,
        qdrant,
        provider,
        "mock-embed",
        0.7,
        0.3,
        Arc::new(TokenCounter::new()),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_semantic_memory_creates_successfully() {
        let memory = mock_semantic_memory().await;
        assert!(memory.is_ok(), "mock_semantic_memory should not fail");
    }
}
