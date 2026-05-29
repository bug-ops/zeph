// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::LlmProvider as _;

use crate::error::MemoryError;

use super::{CORRECTIONS_COLLECTION, SemanticMemory};

impl SemanticMemory {
    /// Store an embedding for a user correction in the vector store.
    ///
    /// Silently skips if no vector store is configured or embeddings are unsupported.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation or vector store write fails.
    pub async fn store_correction_embedding(
        &self,
        correction_id: i64,
        correction_text: &str,
    ) -> Result<(), MemoryError> {
        let Some(ref store) = self.qdrant else {
            return Ok(());
        };
        if !self.effective_embed_provider().supports_embeddings() {
            return Ok(());
        }
        let embedding = match tokio::time::timeout(
            self.embed_timeout,
            self.effective_embed_provider().embed(correction_text),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(MemoryError::Llm(e)),
            Err(_) => {
                tracing::warn!("corrections: embed timed out, skipping vector store write");
                return Ok(());
            }
        };
        let vector_size = u64::try_from(embedding.len()).unwrap_or(896);
        store
            .ensure_named_collection(CORRECTIONS_COLLECTION, vector_size)
            .await?;
        let payload = serde_json::json!({ "correction_id": correction_id });
        store
            .store_to_collection(CORRECTIONS_COLLECTION, payload, embedding)
            .await?;
        Ok(())
    }

    /// Retrieve corrections semantically similar to `query`.
    ///
    /// Returns up to `limit` corrections scoring above `min_score`.
    /// Returns an empty vec if no vector store is configured.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation or vector search fails.
    pub async fn retrieve_similar_corrections(
        &self,
        query: &str,
        limit: usize,
        min_score: f32,
    ) -> Result<Vec<crate::store::corrections::UserCorrectionRow>, MemoryError> {
        let Some(ref store) = self.qdrant else {
            tracing::debug!("corrections: skipped, no vector store");
            return Ok(vec![]);
        };
        if !self.effective_embed_provider().supports_embeddings() {
            tracing::debug!("corrections: skipped, no embedding support");
            return Ok(vec![]);
        }
        let embedding = match tokio::time::timeout(
            self.embed_timeout,
            self.effective_embed_provider().embed(query),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(MemoryError::Llm(e)),
            Err(_) => {
                tracing::warn!("search_corrections: embed() timed out, returning empty");
                return Ok(vec![]);
            }
        };
        let vector_size = u64::try_from(embedding.len()).unwrap_or(896);
        store
            .ensure_named_collection(CORRECTIONS_COLLECTION, vector_size)
            .await?;
        let scored = store
            .search_collection(CORRECTIONS_COLLECTION, &embedding, limit, None)
            .await
            .unwrap_or_default();

        tracing::debug!(
            candidates = scored.len(),
            min_score = %min_score,
            limit,
            "corrections: search complete"
        );

        let mut results = Vec::new();
        for point in scored {
            if point.score < min_score {
                continue;
            }
            if let Some(id_val) = point.payload.get("correction_id")
                && let Some(id) = id_val.as_i64()
            {
                let rows = self.sqlite.load_corrections_for_id(id).await?;
                results.extend(rows);
            }
        }

        tracing::debug!(
            retained = results.len(),
            "corrections: after min_score filter"
        );

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    use crate::embedding_store::EmbeddingStore;
    use crate::in_memory_store::InMemoryVectorStore;
    use crate::semantic::SemanticMemory;
    use crate::store::SqliteStore;
    use crate::token_counter::TokenCounter;

    async fn mem_with_slow_embed(embed_delay_ms: u64) -> SemanticMemory {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();
        let qdrant = EmbeddingStore::with_store(Box::new(InMemoryVectorStore::new()), pool);
        let base_provider = AnyProvider::Mock(MockProvider::default());
        let slow_embed =
            AnyProvider::Mock(MockProvider::default().with_embed_delay(embed_delay_ms));
        SemanticMemory::from_parts(
            sqlite,
            Some(Arc::new(qdrant)),
            base_provider,
            "test-model",
            0.7,
            0.3,
            Arc::new(TokenCounter::new()),
        )
        .with_embedding_provider(slow_embed)
    }

    /// `embed()` timeout in `store_correction_embedding` → returns `Ok(())` (fail-open, skips write).
    #[tokio::test]
    async fn store_correction_embedding_embed_timeout_is_ok() {
        // Build memory before pausing time — SQLite pool uses tokio timers internally.
        let mem = mem_with_slow_embed(10_000).await;

        tokio::time::pause();

        let fut = mem.store_correction_embedding(42, "I prefer detailed answers");
        let (result, ()) = tokio::join!(fut, async {
            tokio::time::advance(std::time::Duration::from_secs(6)).await;
        });

        assert!(
            result.is_ok(),
            "embed timeout must return Ok(()) (fail-open, skip write), got {result:?}"
        );
    }

    /// `embed()` timeout in `retrieve_similar_corrections` → returns `Ok(vec![])` (fail-open).
    #[tokio::test]
    async fn retrieve_similar_corrections_embed_timeout_returns_empty() {
        let mem = mem_with_slow_embed(10_000).await;

        tokio::time::pause();

        let fut = mem.retrieve_similar_corrections("prefer concise answers", 5, 0.7);
        let (result, ()) = tokio::join!(fut, async {
            tokio::time::advance(std::time::Duration::from_secs(6)).await;
        });

        match result {
            Ok(rows) => assert!(
                rows.is_empty(),
                "embed timeout must return empty vec (fail-open), got {rows:?}"
            ),
            Err(e) => panic!("embed timeout must not propagate error, got {e:?}"),
        }
    }
}
