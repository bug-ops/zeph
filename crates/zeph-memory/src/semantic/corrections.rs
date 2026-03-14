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
        if !self.provider.supports_embeddings() {
            return Ok(());
        }
        let embedding = self
            .provider
            .embed(correction_text)
            .await
            .map_err(|e| MemoryError::Other(e.to_string()))?;
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
    ) -> Result<Vec<crate::sqlite::corrections::UserCorrectionRow>, MemoryError> {
        let Some(ref store) = self.qdrant else {
            tracing::debug!("corrections: skipped, no vector store");
            return Ok(vec![]);
        };
        if !self.provider.supports_embeddings() {
            tracing::debug!("corrections: skipped, no embedding support");
            return Ok(vec![]);
        }
        let embedding = self
            .provider
            .embed(query)
            .await
            .map_err(|e| MemoryError::Other(e.to_string()))?;
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
